# Sink 架构重构(方案 A:结构修复,无回归)

## 目标

结构性消除反复出现的 sink-mutex 死锁类(用户说的"总是出现"),
同时修 ISP/DIP 两个 SOLID 问题,**cancel 语义不变、无行为回归**。

## 根因回顾

死锁类 = **sink 互斥锁跨 await 持有 + 同 task 上另一个竞争者挡住 holder 被 poll**。
已发生两次:
1. turn 握 guard 跨整个 turn,工具 re-lock。(已由 `SinkHandle` 修。)
2. 工具握锁跨 `spawn_blocking(stdin)`,`drain`(同 task,select 循环)re-lock
   -> select 提交到 drain 分支(等锁),不再 poll `exec`(工具)-> 工具永不被 poll,
   锁永不释放。(当前 bug。)

**结构触发点**:`drain` 跑在 turn task 上(`try_execute` 的 `select!` 循环里)。
工具握锁跨长 await 时,select 提交到 drain 分支并停止 poll exec -> 死锁。

## 设计(方案 A)

1. **把 drain 移出 turn task(结构修复)**:`try_execute` spawn 一个独立 drain task,
   持有 `Arc<Mutex<dyn EventSink>>` 克隆 + tracer receiver,把 NodeStart/NodeEnd 泵给 sink。
   `try_execute` 退化为 `execute_with(...).await`(无 select 循环)。exec 结束后 `drop(tracer)`
   并 `await` drain handle(排空 + 保序)。这样工具握锁跨 await 不会饿死 turn task
   (同 task 竞争者没了;工具间在 turn task 上是协作式,不会互锁)。
2. **ISP:拆 `PathResolver`**:`PathResolver` 只留 `resolve_path`。新增 `Sink` trait
   (`shell_confirm` / `shell_output`,async,默认空实现)。`allow_shell` 变成 `ToolCtx` 上的 bool。
   工具收 `ToolCtx { resolver, sink, allow_shell }`,不再收 `&dyn PathResolver`。
3. **DIP:sink 改 owned API**:`run_turn` / `run_turn_cancellable` 收 `Box<dyn EventSink + Send>`
   (owned),session 内部包 `Arc<Mutex>`(实现细节)。调用方不再构造 `Arc<Mutex<>>`。
4. 类型别名 `ToolFn` 替换冗长的工具闭包类型(顺带清掉 clippy "very complex type")。

**cancel 不变**:turn 级 emit(ObservationEmitted 等)仍同步(turn task 上锁、emit、返回)。
cancel 测试从 emit 触发 cancel、期望"当轮后停"——仍成立(drain 独立不影响 turn 级 emit 同步性)。

## 改动点(文件级)

### `crates/agnes-builtins/src/tools.rs`
- `PathResolver` trait:删 `allow_shell` / `emit_shell_confirm` / `emit_shell_output`,只留 `resolve_path`。
- 新增 `pub trait Sink: Send + Sync`:`shell_confirm<'a>(&'a self, command: String, responder: oneshot::Sender<bool>) -> BoxFuture<'a, ()>`、
  `shell_output<'a>(&'a self, line: String, is_stderr: bool) -> BoxFuture<'a, ()>`(默认空实现)。
- 新增 `pub struct ToolCtx<'a> { pub resolver: &'a (dyn PathResolver + Send + Sync), pub sink: &'a (dyn Sink + Send + Sync), pub allow_shell: bool }`。
- `Tool::call<'a>(&'a self, args, ctx: &'a ToolCtx<'a>) -> BoxFuture<'a, Result<Value, String>>`;blanket impl 同步改 `Fn(HashMap, &'a ToolCtx<'a>)`。
- `pub type ToolFn = Box<dyn for<'a> Fn(HashMap<String, Value>, &'a ToolCtx<'a>) -> BoxFuture<'a, Result<Value, String>> + Send + Sync + 'static>;`
  各工具闭包类型注解用它替换。
- 工具闭包参数 `|args, resolver|` -> `|args, ctx|`:
  - read-file / write-file / parse-path:`ctx.resolver.resolve_path(...)`
  - shell-run:`ctx.allow_shell`、`ctx.sink.shell_confirm(...)`、`ctx.sink.shell_output(...)`
  - join-lines / llm / summarize / translate:`_ctx`
- `lib.rs` 导出 `Sink`、`ToolCtx`、`ToolFn`。

### `crates/agnes-runtime/src/{lib.rs, scheduler.rs}`
- `execute_with(dag, reg, dispatch, ctx: &ToolCtx<'_>, tracer)`(替换 `resolver`)。
- 所有 eval 函数(`eval_node`/`eval_input`/`eval_expr`/`bind_tool_args`/`call_native`/`call_native_traced`/`collect_kwargs`)
  把 `resolver: &dyn PathResolver` 换成 `ctx: &ToolCtx<'_>`,内部透传 `ctx`。
- `call_native`:`f.call(args, ctx)`。

### `crates/agnes-session/src/session.rs`
- `run_turn` / `run_turn_cancellable`:签名 `sink: Box<dyn EventSink + Send>`;内部
  `let arc = Arc::new(Mutex::new(sink)); self.current_sink = Some(arc.clone());`;`SinkHandle` 仍用于 turn 级 emit。
- `try_execute`:删 select 循环。spawn drain task(持 `self.current_sink.clone().unwrap()` + tracer `rx`,
  循环 `rx.recv().await` -> `arc.lock().await.emit(ev).await`)。构造
  `let ctx = ToolCtx { resolver: &*self, sink: &*self, allow_shell: self.allow_shell };`,
  `execute_with(&dag, &turn_registry, &self.dispatch, &ctx, &tracer).await`;`drop(tracer)`;`drain_handle.await`。
- Session:`impl PathResolver`(只 `resolve_path`);`impl Sink`(`shell_confirm`/`shell_output` 经 `current_sink` 锁 emit)。
  删除原 PathResolver impl 里的 `emit_shell_*`。

### `crates/agnes-session/src/tracer_bridge.rs`
- `drain` 函数不再被 `try_execute` 调用(逻辑内联进 spawned task)。删除 `drain` 或保留给 task 体用。
  `ChannelTracer` 不变。

### `crates/agnes-session/src/events.rs`
- `SinkHandle` 保留(turn 按需加锁)。`SessionEvent` 不变。

### `crates/agnes-cli/src/{chat.rs, run_cmd.rs}`
- sink 构造:`Box::new(StderrEventSink::new())` 替换 `Arc::new(Mutex::new(...))`。
  (chat.rs 2 处:REPL turn + `/run`;run_cmd.rs 1 处。)

### `crates/agnes-cli/src/sink_stderr.rs`
- 不变(`ShellConfirm` 的 `spawn_blocking` 保留;drain 独立后已无死锁)。

### 测试
- `session_end_to_end.rs`:~8 处 `run_turn` 调用改 `Box::new(rec)`,用 `rec.shared()` 保留检查句柄。
- `cancel.rs`:2 处改 Box API;sink 不变;cancel 语义不变。
- `shell_run.rs`:3 处改 `Box::new(AutoApproveSink)`;`AutoApproveSink` 不变(impl `EventSink`)。
- `event_non_exhaustive.rs`:不变。

## 验证

- `cargo build --workspace` 0 警告。
- `cargo test` 全绿(185+;cancel 测试行为不变)。
- 手测:`agnes chat --allow-shell` 跑 `(tool shell-run "cargo build")`——确认提示出现、输入 Y、
  编译实时流式、正常结束,不卡。
- 死锁回归:`shell_run_completes_and_streams_during_turn`(10s 超时)仍通过。

## 提交

单个提交:`refactor(session): decouple sink drain from turn task, split PathResolver (ISP), owned sink API (DIP)`。
(如需要可拆成 drain-独立 + ISP/DIP 两个,但 ToolCtx 跨 builtins/runtime/session,拆开中间态编译较碎,建议单提交。)

## 风险 / 非目标

- **OCP 仅部分修**:新增"工具发出的 sink 事件"仍需 `Sink` trait 方法 + Session impl + 事件变体 + sink arm。
  `Sink` trait 只有 2 个稳定方法(shell_confirm / shell_output),可接受。完全 OCP(工具发任意事件)
  需把 `SessionEvent` 下沉到低层 crate(打破 session→builtins 依赖方向),本次不做。
- **内部仍用 mutex**:DIP 在 API 边界修了(调用方不见 `Arc<Mutex>`);内部 mutex 是实现细节。
  死锁类是靠"移除同 task 竞争者"结构性消除的,不是靠去掉 mutex。
- **ToolCtx 生命周期**:`ToolCtx<'a>` 借用 trait 对象,scheduler 透传 `&ToolCtx<'_>`,
  与现有 `&'a (dyn PathResolver + Send + Sync)` 生命周期模式同构,实现时按需调整。

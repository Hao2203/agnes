# Agnes Agent Loop — Design

> **状态**：草案，待用户审阅。
> **前置工作**：`2026-07-19-agnes-real-llm-integration` 已完成（interactive `agnes chat` REPL + Planner + Session）。本设计在其之上加建。

## 1. 目标

把 `agnes chat` 从"NL → 一段 DSL → 一次执行 → 结束"的**一次性规划器**改造为"NL → 多轮 (DSL → 执行 → observation 回传) → 直到得到最终结果"的**多轮 agent loop**，同时保留 agnes 的核心哲学：**LLM 的输出永远是结构化 DSL，而不是 tool_use**。

具体来说，用户在 REPL 里说一句自然语言，之后：

1. LLM 输出一段 DSL。
2. Runtime 执行这段 DSL，产出一个 `Value`（带 `data + declared_type`）。
3. **根据 `Value.declared_type` 的最外层 head 分派**：
   - `Observation a` → 通过 `a::show(data)` 序列化成字符串，作为一条 observation message 塞回 planner 的 message history，进入下一轮。
   - `Finish a` → 通过 `a::show(data)` 序列化成字符串，作为最终答复展示给用户，本 REPL turn 结束。
   - **其他类型**（`PlainText`、`Summary`、`(List _)`、任何未标签化的类型）→ 隐式当作 `Finish`：通过 registry 的 show 序列化后展示给用户，本 REPL turn 结束。
4. 兜底：`MAX_TURNS = 20` 轮后强制终止。

失败（parse / check / compile / execute 任一环节报错）作为 observation 回传给 LLM，让它自己决定重试还是换方案。

**关键设计选择**：`Finish` / `Observation` 是**可选的意图标签**，不是强制约束。不加标签的 DSL 保持现在的行为（结果直接展示给用户），使这份设计对现有 `examples/*.agnes` 完全向后兼容。只有想让 LLM 继续接力时才必须写 `observe`。

## 2. 核心语言变化

### 2.1 两个新参数化类型

在 `agnes-types` / `agnes-registry` 中新增两个 App-based 类型（与 `List T` / `Option T` 同级）：

```
Finish a       -- runtime 用它标注 "把 a::show(data) 交给用户，明确终结 turn" 的 Value
Observation a  -- runtime 用它标注 "把 a::show(data) 作为 observation 交给 LLM" 的 Value
```

canonical shape：
- `TypeExpr::App { head: TypeName("Finish"), args: vec![a] }`
- `TypeExpr::App { head: TypeName("Observation"), args: vec![a] }`

**关键**：这两个类型**只在运行时**出现在 `Value.declared_type` 里——`finish` / `observe` 的 native_dispatch 实现负责改写。既然它们是可选的意图标签（§1），checker 层**不做**任何根形状检查——完全不加新规则。所有分派逻辑都在 Session 层，读 runtime 返回的 `Value.declared_type` 的最外层 head。

### 2.2 两个新 builtin tool

```
finish  :input Unknown  ->  Unknown  ;; runtime 把返回值的 declared_type 改成 Finish a
observe :input Unknown  ->  Unknown  ;; runtime 把返回值的 declared_type 改成 Observation a
```

两者的静态签名都是 `Unknown -> Unknown`（因为 agnes **不支持类型变量 / 泛型 tool signature**——参见 §2.4 讨论）。**语义**上它们是恒等：把上游数据原样传过，但 native_dispatch 实现会在返回时把 `Value.declared_type` 从 `T` 改写为 `App{head: Finish, args: vec![T]}` 或 `App{head: Observation, args: vec![T]}`。所以 runtime 才是实际决定 `Finish a` 里 `a` 是谁的地方。

LLM 使用这两个 tool 的**语义**：

- `finish` — 明确告诉 Session"我说完了，把这段结果给用户"。跟"结果不加标签直接展示"行为一致，但**观察性更好**——LLM 明示了意图，也让 CLI 能在 stderr 上打个"✓ finish"之类的标记（对比没标签时的"↓ result"）。
- `observe` — 明确告诉 Session"我想看到这个结果，然后决定下一步"。**这个才是必须显式写的**——否则 Session 默认按"隐式 finish"处理，把结果给用户、退出 loop。

**"其他类型"（不加标签）的语义**：等价于隐式 `finish`。这让现有 `examples/*.agnes` 无需改动。

用法示例：

```agnes
;; 一段现有的 DSL，不加任何标签——结果直接展示给用户（隐式 finish）
(pipe (tool read-file :path "README.md") (tool summarize))

;; 明确标注 finish：语义等价，但 LLM 意图清晰
(pipe (tool read-file :path "README.md") (tool summarize) finish)

;; 观察后接力：让 LLM 看到 summary，下一轮再决定要不要翻译
(pipe (tool read-file :path "README.md") (tool summarize) observe)

;; 直接把字符串字面量交给 finish
(pipe "任务完成，已翻译。" finish)
```

### 2.3 Show typeclass

DSL 中的每个类型都需要能被序列化成"给人看/给 LLM 看"的字符串。为此在 `agnes-registry` 中新增 **Show typeclass** 机制：

### 2.3 Show typeclass

DSL 中的每个类型都需要能被序列化成"给人看/给 LLM 看"的字符串。为此在 `agnes-registry` 中新增 **Show typeclass** 机制：

```rust
// agnes-types
pub type ShowFn = fn(&JsonValue) -> String;

// agnes-registry
pub struct Registry {
    // ...existing fields...
    shows: HashMap<TypeName, ShowFn>,
}

impl Registry {
    /// 注册一个类型的 show 实现。跟 register_type/register_alias/register_tool
    /// **正交**——一个类型可以先注册（有/无 validator）再注册 show，也可以只
    /// 注册 show 不注册 type。冲突判定只看 `shows` 表本身：重复 register_show
    /// 同名 → RegistryError::DuplicateShow { name }。**不**走 `ensure_free`。
    pub fn register_show(&mut self, name: &str, f: ShowFn) -> Result<(), RegistryError>;

    pub fn show_of(&self, name: &TypeName) -> Option<ShowFn>;

    /// 递归 show：对 App 类型（List T、Finish T、Observation T、Option T）
    /// 先看外层 head 有没有注册 show（一般不会），没有就按 §5 递归规则展开
    /// 到内部类型。
    pub fn show_value(&self, value: &Value) -> String;
}
```

builtins 提供内置类型的默认 show：

| 类型 | show 输出 |
| --- | --- |
| `PlainText` | `data.as_str()`（原样） |
| `Summary` | `data.as_str()`（原样） |
| `TranslatedText` | `data.as_str()` |
| `Markdown` | `data.as_str()` |
| `PDF` | `<PDF binary, N bytes>`（占位，因为 PDF 不该整体上下文化） |
| `Unit` | `""`（空） |
| `List T` | `"["` + `T.show(x)` 逗号连接 + `"]"` |
| `Option T` | `T.show(x)` 或 `""` |
| `Finish T` / `Observation T` | 递归到 `T.show` |
| 未注册的 `Named` | `serde_json::to_string_pretty(data)` 兜底 |

未来自定义类型的作者应该注册自己的 show；不注册就用兜底。checker **不强制**要求每个类型有 show（避免过度约束），Session 层在需要序列化时才落地。

## 3. Session 循环重构

### 3.1 现在的循环 vs 新循环

**现在**（`agnes-session/src/session.rs`）：

```
run_turn(input):
  input = NL? → planner.plan(nl) → dsl_source
  input = RawDsl? → dsl_source = raw
  parse → check → compile → execute → Value
  emit TurnResult(show(Value))
```

**新**（改造后）：

```
run_turn(input):
  # 1. seed
  planner.begin_user_turn(input)
  seeded_dsl = match input {
    NL(_)      => None
    RawDsl(s)  => Some(s)   // 第 0 轮直接跑用户提供的 DSL；跳过第一次 plan_next
  }
  for iter in 0..MAX_TURNS:
    emit IterationStart{iter}
    dsl_source = match seeded_dsl.take() {
      Some(s) => s          // 只有 iter=0 且 RawDsl 时用到
      None    => planner.plan_next()?
    }
    result = try_execute(dsl_source)   // parse → check → compile → execute → Value
    match result {
      Ok(value) => match classify_root(&value) {
        RootKind::Observation => {
          let inner = extract_inner_type(&value.declared_type)  // Observation 剥一层
          let raw = registry.show_value(&value)
          let obs_text = truncate_if_over_8000(raw)
          emit ObservationEmitted{iter, text: obs_text.clone(), is_error: false}
          planner.push_observation(dsl_source, obs_text, false, Some(inner))
          continue
        }
        RootKind::Finish | RootKind::Other => {
          // Finish 显式意图 or 未标签的普通类型——都当作终结。
          let s = registry.show_value(&value)  // Finish 递归剥一层；Other 直接 show
          emit TurnResult{value: s.clone()}
          planner.record_finish(dsl_source, s)
          return Ok(value)
        }
      }
      Err(e) => {
        let text = format!("{phase}: {e}")  // phase = "parse"/"check"/"compile"/"execute"
        emit ObservationEmitted{iter, text: text.clone(), is_error: true}
        planner.push_observation(dsl_source, text, true, None)
        continue
      }
    }
  # 2. exhaustion
  planner.abandon_pending_turn()
  emit TurnFailed{error: "MAX_TURNS ({MAX_TURNS}) reached"}
  return Err(TurnLimitExceeded)
```

**RawDsl 走同一循环**：`/run <dsl>` 只影响第 0 轮的 DSL 来源（用户提供，跳过 planner 首次调用）。如果 raw DSL 求值成 `Observation _`，Session **仍然**把 observation 塞回 planner 并进入第 1 轮——用户手工调 `observe` 相当于把控制权正式交给 planner。如果求值成其他类型（含 `Finish`），直接展示给用户结束。这让 RawDsl 跟 NL 完全对称，不再是"特殊路径"。

### 3.2 MAX_TURNS

固定 `MAX_TURNS = 20`。可通过 CLI 覆盖：`--max-turns <N>`（新增 flag）。

选择 20 的理由：Claude Code / LangGraph plan-and-execute 主流默认 20-25；每轮的 DSL 可以包含 pipe/par 组合多个 tool，因此 20 轮承载的实际工具调用远多于 20。

### 3.3 新增/变更的 SessionEvent

现有变体全部保留。新增：

```rust
pub enum SessionEvent {
    // ...existing (PlannerStart, DslProduced, PlanReady, NodeStart, NodeEnd,
    // TurnResult, TurnFailed, PlannerRetry, WriteSummary)...

    /// Entering iteration `iter` (0-indexed) of the agent loop.
    /// Fires once per iteration, immediately before PlannerStart of that iter.
    IterationStart { iter: u32 },

    /// The LLM (or a runtime error) produced a piece of information that will
    /// be fed back into the next planner call. Fires whenever the loop decides
    /// to continue rather than terminate.
    ObservationEmitted {
        iter: u32,
        text: String,
        is_error: bool,
    },
}
```

`PlannerRetry` 现在语义变了：它现在只在**一次 LLM 调用本身失败**（如 HTTP 错误、EmptyResponse）时触发；DSL 出错走 `ObservationEmitted { is_error: true }`。

## 4. Planner 重构

### 4.1 History 语义

现在一个 `Turn` 是 `{ user_nl, assistant_dsl, result_preview }`。新版：

```rust
pub struct Turn {
    pub user_nl: String,
    pub iterations: Vec<Iteration>,
    pub outcome: TurnOutcome,
}

pub struct Iteration {
    pub assistant_dsl: String,
    pub observation: Option<Observation>,  // Some 表示循环继续；None 表示终结
}

pub struct Observation {
    pub text: String,
    pub is_error: bool,
    /// Value.declared_type 的**内层**类型（Finish/Observation 剥掉一层），
    /// 用于填 message 里的 XML `type="..."` 属性。
    /// error 观察时为 None（走 `error="true"` 分支）。
    pub type_name: Option<TypeName>,
}

pub enum TurnOutcome {
    Finished { result: String },
    TurnLimitExceeded,
}
```

`Planner::history() -> &[Turn]` 保持返回 `Turn` 切片。`/history` 命令要打印一个层级化列表：每个 turn 展开成多次迭代，每次迭代显示 DSL + observation。

### 4.2 message 构造

Planner 每次调用 LLM 时构造的 `CompletionRequest.messages` 结构（**参考 Claude tool_use 的 tool_result block 形式**）：

```
system:  <base_system_prompt + prior context summary (if >6 turns)>

历史 turns (最近 6 个 verbatim)：
  ...
  user: <user_nl of turn N-2>
  assistant: <iterations[0].assistant_dsl>
  user: <observation from iteration 0, XML-wrapped as below>
  assistant: <iterations[1].assistant_dsl>
  user: <observation from iteration 1>
  ...
  assistant: <last iterations[last].assistant_dsl (this was the Finish)>
  user: <NEXT turn user_nl>
  ...

当前 turn (in-flight)：
  user: <current user_nl>
  assistant: <iteration 0 DSL>
  user: <iteration 0 observation, XML-wrapped>
  assistant: <iteration 1 DSL>
  ...
```

Observation XML 包装：

```xml
<observation type="Summary">
[serialized content here, produced by Summary.show()]
</observation>
```

失败版本（error observation）：

```xml
<observation error="true" phase="check">
Type mismatch at (pipe read-file summarize):
  Why: pipe stage 2 expected PlainText but got Summary.
  Fix: insert (tool render-text) between summarize and finish, or restructure.
</observation>
```

`type` 属性是 `Value.declared_type` 的内层类型（比如 `Finish Summary` → `type="Summary"`）。error 版本没有 type，改用 `error="true"` + `phase="parse|check|compile|execute"`。

**注意**：LLM 视角下所有 observation 都是 `role: user` 的消息（这是 Anthropic API 的约定：非 assistant 输出的一切都通过 user 角色回传）。这样保证 role 交替，不触发 F1 类问题。

### 4.3 Planner 接口变化

```rust
impl Planner {
    // 保留：
    pub fn new(provider: Arc<dyn Provider>, registry: &Registry) -> Self;
    pub fn history(&self) -> &[Turn];
    pub fn reset_history(&mut self);
    pub fn abandon_pending_turn(&mut self);  // F1 fix，保留

    // 移除或改名：
    // pub fn plan(&mut self, nl: &str) -> Result<String, PlannerError>;
    // pub fn push_error_feedback(&mut self, bad_dsl: String, err: String);
    // pub fn record_result(&mut self, dsl: String, result_preview: String);

    // 新增：
    pub fn begin_user_turn(&mut self, nl: String);
    pub fn plan_next(&mut self) -> Result<String, PlannerError>;
    /// 通用观察回传。构造一个 `Iteration { assistant_dsl: dsl,
    /// observation: Some(Observation { text, is_error, type_name }) }`
    /// 追加到当前 in-flight turn 的 iterations 里，供下一次 plan_next 使用。
    /// - is_error=false：type_name 应该是 Value.declared_type 内层类型（Finish/Observation 剥一层后）；
    /// - is_error=true：type_name 传 None；phase（"parse"/"check"/"compile"/"execute"）由 Session 拼进 text。
    pub fn push_observation(
        &mut self,
        dsl: String,
        text: String,
        is_error: bool,
        type_name: Option<TypeName>,
    );
    /// 收到 Finish（显式）或**未标签的普通类型**（隐式 finish）：把当前
    /// in-flight iterations（最后一次的 observation=None）和最终 result
    /// 一起 commit 到 history。result 是 show_value 出来的字符串。
    /// Planner 不区分显式 vs 隐式——只关心 turn 是否 commit。
    pub fn record_finish(&mut self, dsl: String, result: String);
}
```

### 4.4 system prompt 大改

现在 prompt 讲的是"输出一段 agnes DSL"。新 prompt 要讲：

- 你是一个 agent。每一轮你写一段 agnes DSL；执行结果作为 `<observation>` 回来。
- DSL 的**根**必须是 `finish` 或 `observe`。想终结这个用户 turn，用 `finish`；想继续观察结果、下一步再决定，用 `observe`。
- 一段 DSL 可以包含 pipe / par / define / if / match / foreach / list literal / 变量，跟以前一样。
- 7 个 builtin tool 目录（同现有）+ 2 个新 tool：`finish`, `observe`。
- 错误观察是 `<observation error="true" phase="..."/>`；LLM 应该读它、修 DSL、下一轮再试。
- 观察太长会被截断（见 §5）。
- Few-shot：至少 3 个例子——(a) 简单一轮 `finish`；(b) 两轮 `observe → finish`；(c) 出错后自我修正的两轮。

## 5. 序列化与截断

`show_value(&Value)` 递归实现：
1. 先看 `value.declared_type` 的最外层 head 有没有注册 show；有就用。
2. 否则如果是 `App { head: "|", args }`（union）：按运行时数据形状挑一个成员的 show。
3. 否则如果是 `App { head, args }`：如果 head 是内置容器（`List`, `Option`, `Finish`, `Observation`），走内建组合规则；否则用兜底。
4. `Named` 未注册时 → `serde_json::to_string_pretty(data)`。

**Observation 截断**：单次 observation 的字符串输出如果超过 8000 字符，Session 层截断中段并加提示：

```
<observation type="PlainText">
[first 4000 chars]

... [truncated 12345 chars — full length: 20345] ...

[last 4000 chars]
</observation>
```

理由：防止 LLM 上下文被单个 observation 吃光。8000 字符约 2000-4000 token（英文 ≈ 2000，中文 ≈ 4000），跟 Anthropic 建议的 tool_result 上限对齐。**Finish** 不截断——那是给用户看的最终答复，用户想看全。

## 6. CLI 显示

`StderrEventSink` 增加：

- `IterationStart { iter }` → 打印分隔线：`── iteration 3 ──`
- `ObservationEmitted { iter, text, is_error }` → 打印：`↓ observation (iter 3, 156 chars)` + 若 `is_error` 用红色前缀

`plan_view` 不变（每轮各自渲染 plan tree）。

REPL 循环（`agnes chat` 主循环）：
- 用户输入 → `session.run_turn(TurnInput::NaturalLanguage(nl))`
- Session 内部跑完 loop，一路 emit events
- 最终 `TurnResult` / `TurnFailed` 到达 → REPL 打印 `Value.data` 到 stdout（现有行为），继续下一次读入

`/history` 更新：显示每个 turn 的**所有** iteration（DSL + observation + 最终 Finish）。

`--max-turns` 新增 CLI flag：`agnes chat --max-turns 30`，默认 20。

## 7. 分层职责矩阵

| 层 | 变更 |
| --- | --- |
| `agnes-ast` | 无。 |
| `agnes-parser` | 无（`finish` / `observe` 走普通 `(tool ...)` 语法）。 |
| `agnes-types` | 新增 `pub type ShowFn = fn(&JsonValue) -> String;`。`TypeExpr` 无变化（`Finish`/`Observation` 作为 App head 已由现有 `App { head, args }` shape 支持）。 |
| `agnes-registry` | 新增 `shows: HashMap<TypeName, ShowFn>` + `register_show` + `show_of` + `show_value(&Value)`；`RegistryError` 新增 `DuplicateShow { name }` 变体。`resolve` **无变化**——`Finish`/`Observation` 是普通 head，跟 `List` 处理路径相同。 |
| `agnes-checker` | **无变化**。不加新规则。Finish/Observation 是运行时标签，不是静态类型约束。 |
| `agnes-compiler` | 无变化（`finish` / `observe` 是普通 tool 节点）。 |
| `agnes-builtins` | 注册 `finish`, `observe`（signature 都是 `Unknown -> Unknown`）。注册所有内置类型的 show（PlainText/Summary/TranslatedText/Markdown/PDF/Unit）+ 组合规则（List/Option）走 `show_value` 内建。`native_dispatch` 新增 `finish` / `observe` 的实现：**恒等 + declared_type 改写**为 `App{Finish/Observation, [inner]}`。 |
| `agnes-runtime` | 无变化。scheduler 现有能力已足够——`finish`/`observe` 是普通节点，返回 Value 时 `declared_type` 被 native_dispatch 改写。 |
| `agnes-llm` | Planner 大改：新接口 + 新 history + 新 system prompt + 新 few-shot。`extract_dsl` 保留不变。 |
| `agnes-session` | 大改：`SessionEvent` **先加 `#[non_exhaustive]`**（作为独立的准备提交，方便后续新增变体），然后新增 `IterationStart` / `ObservationEmitted`；`run_turn` 重构为 loop + observation XML 包装 + 截断 + MAX_TURNS + 错误回传。Session 内加 helper `classify_root(value: &Value) -> RootKind { Finish, Observation, Other }`（读 Value.declared_type 最外层 head）。 |
| `agnes-cli` | `StderrEventSink` 新增两个事件的渲染；`--max-turns` flag；`/history` 显示更新。**现有 `examples/*.agnes` 无需改动**——不加标签的 DSL 走隐式 finish 路径。 |

## 8. 错误处理与状态一致性

### 8.1 每轮失败的处理

Parse / check / compile / execute 任何一环失败：
1. Session 捕获错误，转成人类可读文本（What/Why/Fix）。
2. 通过 `push_observation(dsl, err_text, is_error=true, None)` 塞回 planner。
3. `emit ObservationEmitted { iter, text, is_error: true }`。
4. 进入下一轮。

**不**在 planner 层做重试。RetriesExhausted 语义并入 MAX_TURNS。

### 8.2 F1 / F2 的适用

F1 (assistant/assistant 连续)：新架构下 assistant 永远只写 DSL，observation 用 user 角色。**天然不违反**。但保留 `abandon_pending_turn` 作为防御性 API，会在 `session.rs` 的 `MAX_TURNS` 触底时调用。

F2 / `pending_nl` 泄漏：新架构下 `begin_user_turn` 显式启动，`record_finish` 显式提交；无隐式状态。保留 `abandon_pending_turn` 处理 MAX_TURNS 触底。

### 8.3 Ctrl-C 语义

`chat` REPL 里用户按 Ctrl-C 触发 rustyline `Interrupted`。行为：
- 如果不在 turn 中：现有行为——清空当前行，继续。
- 如果在 turn 中（Session 正在 loop）：中断 Session；调用 `planner.abandon_pending_turn()`；打印 `(cancelled after N iterations)`。

需要 Session 支持一个 cancellation token。现有 `run_turn` 是 `async fn`——用 `tokio::select!` 加一个 `Cancelled` future 即可。CLI 层用 `Arc<tokio::sync::Notify>`。

## 9. 测试计划

### 9.1 单元测试

- **`agnes-types`**：`ShowFn` 存在性、Registry 的 show 查找、`show_value` 对 List/Option/Finish/Observation 的递归行为。
- **`agnes-registry`**：注册重复 show 应该报冲突；show_of + resolve 联动。
- **`agnes-checker`**：无新规则，无新测试。（Finish/Observation 不进静态类型系统。）
- **`agnes-builtins`**：`finish` / `observe` 的 dispatch 正确改写 declared_type；PDF/List/Option 的 show 输出格式。
- **`agnes-llm::planner`**：`begin_user_turn` + `plan_next` + `push_observation` + `record_finish` 的状态机；message history 的 role 交替不变式；6-turn 收缩仍然正确。

### 9.2 Session 集成测试

用 `MockProvider` 驱动：

- **一轮直接 finish**：mock 返回 `(pipe "done" finish)`；断言 `TurnResult { value: "done" }`。
- **一轮无标签结果（隐式 finish）**：mock 返回 `(pipe (tool read-file :path "x") (tool summarize))`；断言 `TurnResult { value: <summary> }`；断言只有 iter=0 一轮。
- **两轮 observe → finish**：mock 依次返回 observe DSL、finish DSL；断言 iter=0 emit ObservationEmitted，iter=1 emit TurnResult；断言 planner message history 里第 3 条是 XML observation。
- **两轮 observe → 无标签**：mock iter 0 返回 `(pipe ... observe)`，iter 1 返回不加标签的 pipe；断言 iter=1 走隐式 finish 路径展示给用户。
- **执行错误自愈**：mock iter 0 返回 checker 会拒绝的 DSL（比如类型不匹配），iter 1 返回正确 DSL；断言 iter=0 emit ObservationEmitted{is_error=true}，iter=1 完成。
- **MAX_TURNS 触底**：mock 永远返回 observe DSL；断言 20 轮后 emit TurnFailed{error: contains "MAX_TURNS"}；断言 planner scratch 已被 `abandon_pending_turn` 清空。
- **RawDsl + 无标签**：`/run (pipe (tool summarize))` 应该 iter 0 求值后直接 TurnResult 展示（隐式 finish）。
- **RawDsl + Observation → 接力**：`/run (pipe (tool summarize) observe)` 应该 iter 0 emit ObservationEmitted，iter 1 让 planner 出手接力；断言进入了第二轮。
- **RawDsl + Finish**：`/run (pipe (tool summarize) finish)` 显式 finish 也走 TurnResult 展示，跟无标签路径一致。

### 9.3 CLI 快照/端到端

- `plan_view` 快照测试：多轮 iteration 时 stderr 输出结构（`── iteration 2 ──` 分隔、observation 行的格式）。
- Acceptance（现有 `acceptance.rs`）：**保持不变**。之前的"跑一段 DSL 就完"现在走隐式 finish 路径，行为一致。

### 9.4 手工验证

沿用 Task 12 的 `examples/chat-demo.md`。新增：
- 单轮 finish 场景（用户："给我读一下 README" → 一段 DSL 就 finish）。
- 多轮 observe → finish 场景（用户："看一下 README 然后总结成日文" → LLM 先读、观察长度、再决定 summarize 或直接翻译）。
- 出错自愈场景（人为让 LLM 产出错 DSL；例如让它使用不存在的 tool；观察下一轮自愈）。

## 10. 迁移影响与向后兼容

**破坏性变更**（列出所有）：

1. `Planner::plan` / `push_error_feedback` / `record_result` 的签名改变（移除，被新接口替代）。
2. `SessionEvent` 新增两个变体（enum 先加 `#[non_exhaustive]`，见 §11 施工顺序 step 9）。
3. Turn 结构改（`assistant_dsl / result_preview` → `iterations`）。`/history` 命令输出格式变化。

**保持兼容**：
- `agnes_runtime::execute(...)` 签名不动。runtime crate 完全不变。
- `agnes-parser` / `agnes-ast` / `agnes-compiler` 完全不变。
- `agnes-checker` **完全不变**——不加新规则。
- **现有 `examples/*.agnes` 完全不用改动**——不加标签的 DSL 走隐式 finish 路径。
- 用户自定义类型的注册流程不变；只是**可选**注册 `show`。
- `MockProvider` 的接口不变；只是 Planner 内部构造 messages 的规则变了。

## 11. 施工顺序（预告）

不是这份 spec 的一部分，只是让读者预估规模。真正的任务拆分由 `superpowers:writing-plans` 产出。粗略预计 9-13 个任务：

1. `agnes-types::ShowFn` 类型别名 + tests。
2. `agnes-registry` 新增 `shows` map + `register_show` + `show_of` + `show_value(&Value)` + `RegistryError::DuplicateShow`。
3. `agnes-builtins` 注册所有现有类型的 show impl（PlainText / Summary / TranslatedText / Markdown / PDF / Unit）。`show_value` 中内建 `List T` / `Option T` / `Finish T` / `Observation T` 组合规则。
4. `agnes-registry::resolve` 验证 `Finish _` / `Observation _` 作为 App head 正确工作（当前 App 处理路径应该已经支持）。
5. `agnes-builtins` 注册 `finish` / `observe` tool（signature `Unknown -> Unknown`）+ native_dispatch 实现（改写 declared_type）+ 测试。
6. `agnes-llm::planner` 新接口 (`begin_user_turn` / `plan_next` / `push_observation` / `record_finish`) + 新 `Turn`/`Iteration`/`Observation` 结构 + 单元测试。
7. `agnes-llm::planner` system prompt 大改 + few-shot 更新 + 单元测试（观察 mock request messages 的 role 交替）。
8. `agnes-session::SessionEvent` 加 `#[non_exhaustive]`（**独立提交**，方便后续新增变体不惊扰 downstream）。
9. `agnes-session` 新增 `IterationStart` / `ObservationEmitted` 变体 + `classify_root` helper + `extract_inner_type`。
10. `agnes-session::run_turn` 重构为 loop + observation XML 包装 + 截断 + MAX_TURNS + 错误回传 + RawDsl 复用同一 loop + 集成测试。
11. `agnes-session` Ctrl-C 取消支持（cancellation token + `run_turn` 感知）。
12. `agnes-cli::sink_stderr` 渲染新事件；`--max-turns` flag。
13. `agnes-cli` `/history` 更新；`examples/chat-demo.md` 更新 + `README.md` 更新 + workspace `cargo test` + `cargo clippy --deny warnings`。

**注意**：checker 无变化。现有 `examples/*.agnes` 无需迁移。这两点比之前版本少了两个改动点。

## 12. 已知遗留 / 明确不做

- **不做**：DSL 内部循环 (`loop-until` / 递归重执行)。控制流权全部交给外层 Session loop。
- **不做**：`observe` 里嵌套 `finish` 之类的复合语义——`classify_root` 只看 `Value.declared_type` 的最外层 head，只有一层"意图"生效。如果 LLM 写了 `(pipe X finish observe)`，runtime 顺序 apply：先 finish 改成 `Finish X`，再 observe 改成 `Observation (Finish X)` — 外层是 Observation 就走 observe 分支，Finish 被埋在内部无副作用。这是"最后一个赢"语义，好懂。
- **不做**：把 tool_result / observation 用 Anthropic tool_use API 传输。仍然用普通 text messages + XML 包装。理由：agnes 的哲学是"DSL 替代 tool_use"；连传输层都不该沾 tool_use API。
- **不做**：LLM 的思考痕迹（chain-of-thought / scratchpad）。若未来要加，是另一份 spec。
- **未来可加**：用户自定义 `show` 注册的 DSL 语法（现在只能 Rust 侧注册）。

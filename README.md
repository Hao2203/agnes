# agnes

终端 AI 编程助手的 Rust 实现，兼容任意 OpenAI 风格的 Chat Completions API。

agnes 采用六边形架构：纯领域核心（类型状态机）与所有 I/O 适配器彻底分离，
模型协议、HTTP 传输与工具均可独立替换。它提供 CLI 二进制 `agnes`，
既能执行一次性任务，也能进入带完整会话记忆的交互式会话。
项目处于早期开发阶段（0.1.0）。

## 特性

- 类型状态机驱动的 agent 循环，非法协议顺序在编译期即不可构造
- 一轮 turn 的生命周期（`turn_start` / `message_*` / `tool_execution_*` /
  `turn_end`）由驱动按序铸造并记入 `Log`；core 只持有领域状态（`Context`），
  消息、系统提示与工具环境都可从日志复原，一次请求也能从日志单独推导
- 宿主扩展内核：插件只处理事件、返回指令，系统在检查点执行；拒绝单调
  （插件无法把拒绝变回放行），注入进队列，可在运行期增删工具与中止，
  注册可回收，装载期校验配置
- 工具调用可走审批流：`allowed-once | rejected | cancelled | unavailable`，
  没接审批口即拒绝（fail-closed）
- 会话日志可落盘：JSONL、单写者、崩溃后补写收尾（绝不重写已有记录）；
  审批决定、插件中止与失败原因作为宿主审计记录与事件共享同一顺序
- 端口与适配器分离：`core` 无 I/O、无异步，替换运行时只需更换适配器
- 边界处一次性解析：模型名、URL、API key、工具名等都是强类型
- 秘密保护：API key 由 `secrecy` 承载，调试输出不泄露明文
- 内置沙箱化的文件工具，以及默认关闭的 shell 工具
- 运行中可中断、可注入消息（用户输入与插件指令都进队列，在 step 边界
  生效），turn 预算防止失控循环

## 快速开始

需要 Rust 1.89+（edition 2024）。

```bash
cd agnes-rs
cargo build --release
export AGNES_API_KEY=sk-...
./target/release/agnes run "解释 core/src/turn.rs 中的状态机"
```

也可以安装到 `PATH`：

```bash
cargo install --path cli
```

## 使用

### 一次性任务

```bash
agnes run "把 src/main.rs 里的 println! 换成日志"
```

省略任务参数时从标准输入读取，便于管道组合：

```bash
echo "列出所有 TODO 注释" | agnes run
```

### 交互式会话

不指定子命令时默认进入 `chat`：

```bash
agnes chat
```

```text
$ agnes chat
agnes> 列出仓库里的所有 crate
...
agnes> /new
(session cleared)
agnes> exit
```

会话中每一行都是一个任务，且共享完整会话记忆：

带 `--session` 时会话落盘，重启后用同一路径即可恢复（崩溃留下的未收尾
事件会被自动补写）：

```bash
agnes chat --session ./notes/session.jsonl
```

- `/new`：清空会话日志（系统提示与工具环境由下一轮重新记录一条 header）；
  带 `--session` 时轮转到新的兄弟文件，因为日志只追加
- `exit`、`quit` 或 EOF：退出会话
- `Ctrl-C`：任务运行中中断任务并保留会话；在提示符处则退出
- 运行期间输入的消息会排队，在下一个 step 边界注入；如果模型已经给出
  答复，收件箱非空时会在同一 turn 内续跑（预算允许时），否则作为下一轮
  任务处理
- 需要审批的工具在终端询问 `[y] once / [n] reject / [a] always this session`

### 命令行选项

以下选项均为全局选项，对子命令同样生效：

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `--model <MODEL>` | `gpt-4o-mini` | 模型名称 |
| `--base-url <URL>` | `https://api.openai.com/v1` | OpenAI 兼容 API 地址 |
| `--api-key <KEY>` | 见环境变量 | API 密钥 |
| `--max-steps <N>` | `32` | 每个任务的最大模型轮次 |
| `--allow-shell` | 关闭 | 向模型提供 `run_shell` 工具 |
| `--session <PATH>` | 关闭 | chat/run 的会话文件：存在即恢复，不存在即新建 |
| `--yes` | 关闭 | 自动批准 ask 规则（审批审计仍会落盘） |
| `--deny <TOOL>` | 无 | 拒绝工具：`name` 精确匹配，`prefix*` 前缀匹配，可重复 |

默认策略：`run_shell`（仅在启用 `--allow-shell` 时）每次执行前在终端询问
`[y] once / [n] reject / [a] always this session`；拒绝会作为一次失败的
工具结果反馈给模型。`run` 模式下需要审批时必须能读终端；管道输入任务或
无 TTY 的环境请加 `--yes`（自动批准）或 `--deny run_shell`，否则启动即报错，
不会挂起等待。带 `--session` 时，`/new` 会轮转到下一个空闲的兄弟文件 ——
日志只追加，从不重写。

### 环境变量

| 变量 | 说明 |
| --- | --- |
| `AGNES_API_KEY` | API 密钥，等价于 `--api-key` |
| `OPENAI_API_KEY` | 未提供密钥时的回退来源 |

## 内置工具

| 工具 | 参数 | 说明 |
| --- | --- | --- |
| `read_file` | `path` | 读取工作目录下的文本文件 |
| `write_file` | `path`、`content` | 创建或覆盖文件，自动创建父目录 |
| `list_dir` | `path`（可选） | 列出目录，子目录以 `/` 结尾 |
| `run_shell` | `command` | 以 `sh -c` 执行命令，需 `--allow-shell` |

所有路径都相对于启动时的工作目录，绝对路径与 `..` 逃逸会被拒绝；
`read_file` 有字节上限，单次工具输出超过 100 000 字符时按 UTF-8 字符边界截断。

## 架构

工作区位于 `agnes-rs/`，按依赖方向分层：`core` 是纯计算，`io` 定义
端口，其余 crate 是适配器与宿主。

依赖方向（箭头指向被依赖的 crate）：

```text
util ← core ← io ← agent ← cli
                      ↖ session-file
```

`agnes-transport-reqwest` 与 `agnes-llm-openai` 是挂在 `io` 之上的适配器；
`agnes-session-file` 挂在 `agnes-agent` 之上，因为持久化事件词汇归宿主。
只有 `agnes-cli` 负责把具体实现组装成可运行的程序。

| crate | 职责 |
| --- | --- |
| `agnes-util` | 零依赖基础类型：`NonEmpty<T>`、`EmptyError` |
| `agnes-core` | 纯领域模型与类型状态机（`Turn` + `Context`），无 I/O、无异步、不含序列化、不产出事件 |
| `agnes-io` | 异步端口 trait：`Llm`、`Tool`、`HttpTransport`、`Input` |
| `agnes-llm-openai` | OpenAI Chat Completions 协议适配，泛型于传输层 |
| `agnes-transport-reqwest` | 基于 `reqwest` 的 HTTP 传输适配器 |
| `agnes-session-file` | 会话日志的 JSONL 实现：单写者、格式版本、崩溃补写；依赖 `agnes-agent` 的记录词汇，线格式（wire）定义在此 |
| `agnes-agent` | 宿主装配（`Host`）、驱动状态机、持久化事件词汇 `record`、`SessionLog` 端口、会话日志 `Log`、扩展内核与内置工具 |
| `agnes-cli` | 二进制 `agnes`：参数解析、REPL、审批与独立写线程的终端渲染 |

### 关键设计

- **类型状态机**：`Turn<AwaitingModel>` → `Turn<AwaitingTools>` →
  `Turn<Closed>`；转移消耗当前状态、交还下一个阶段（或携带下一阶段的
  枚举），阶段本身就是动作提示。`Turn` 持有一个 `Context`（系统提示与
  工具环境、会话消息、待注入消息、用量、step 记账）；core 不持有日志、
  不产出事件，驱动读取 `Context` 并自行铸造、记录事件。
- **宿主扩展内核**：`Plugin` 只有一个处理入口 `on_event`；它看不到派发/
  执行/审批阶段，只返回 `Command`（拒绝 / 审批 / 替换结果 / 注入 / 中止 /
  增删工具）。`Extensions` 把指令收进 outbox，驱动在明确检查点取用：门禁
  前取拒绝与审批、结果入账前取替换、每次模型调用前取工具变更并记录
  新 header、注入队列交给 core 的注入缓冲。指令里没有 `Allow`，拒绝单调；
  长期注册拿 `Registration`，临时注册用 `Scope`（离开作用域逆序撤销）。
  失败 run 里已产生的工具变更对下一 turn 生效；无法执行的定向指令与注入
  丢弃。
- **装载期校验**：`Bundle::install` 在装配时检查规则能否命中已注册工具、
  配了 ask 是否真的接了审批口；配置错误在启动时失败，而不是第一次调用。
  规则可以指向运行期才注入的工具（`PolicyBundle::allow_unresolved`），
  默认仍拒绝永不命中的规则。
- **端口与适配器**：`Llm`、`Tool`、`HttpTransport`、`Input` 是 `io` 的
  `async_trait` 端口；`SessionLog` 随记录词汇归 `agnes-agent`。
  `OpenAiClient<T: HttpTransport>` 不绑定具体传输。
- **非法状态不可表示**：`StopReason::ToolUse` 携带非空的
  `NonEmpty<ToolCall>`，`Content` 保证非空，`ApiKey` 由 `secrecy`
  承载，`ApprovalOutcome` 只有 `AllowedOnce` 会放行。
- **工具失败即输出**：工具错误以 `ToolOutput` 形式回传给模型（拒绝、
  挡回、超时同样如此）；只有模型或传输错误会中止运行。
- **日志即账本**：`Log` 类型位于宿主侧（`agnes-agent`），由驱动持有，是唯一
  的持久化状态；core 只持有领域状态 `Context`，不产出事件。系统提示与工具
  环境是 `Context` 的 header 字段；驱动在每个 turn 开局记录一条
  `record::Event::Header`，工具环境运行中变化时再记一条。消息也是事件
  （`record::Event::Message`），用户输入与插件注入都是带角色的消息事件。
  `Context::prompt()` 把 header、消息与工具表投影成模型输入，因此一次请求
  完全可以从日志复原；`RunSummary.log` 可直接传回下一次运行。
  工具集变化（含插件运行期注入/移除）在下一个模型调用前记一条新 header，
  同一 turn 内生效；在飞调用不受影响。
- **日志耐久**：每个上报事件先追加到会话文件、再上报宿主；每个模型请求
  前与收场时 `flush`（fsync）。格式 v6：首行版本标记，之后是 `record`
  事件行与 `host` 审计行（审批问询/决定、插件中止、失败原因），共享一个
  连续 `seq`；恢复时校验连续性并**补写**未收尾的 step/turn（与
  `Turn::interrupted` 同形），从不重写已有记录；旧格式（含 v5）与重名
  工具环境拒载。`seq` 只在实际写成功后提交，写失败会 poison 会话句柄，
  避免半行之后继续追加。v6 里 `MessageEnd` 只记无载荷的 `StopKind`，
  `StepOutcome`/`Outcome` 不再复制答复文本。
- **审计与可追溯**：`MessageEnd` 记录产生该消息的模型（驱动用自己构造的
  `LlmRequest.model` 记录），切换模型后日志仍能归因；审批落定、插件中止
  与 run 失败原因不扩 core 词汇，作为 `HostEvent` 与事件一起落盘。
- **转换者由宿主决定**：`core` 不组装提示词；驱动器在每次模型调用前用
  宿主侧的 `RequestBuilder`（`agnes-agent`）把 `Context` 转成 `LlmRequest`。
- **格式由上层定义**：core 与 agent 的领域词汇（`Context`、`record::Event`、
  `HostEvent`）不实现序列化 —— 会话的 JSONL 形状由 `agnes-session-file` 的
  `wire` 层定义（同 `agnes-llm-openai` 的 `wire.rs`），校验与拒载规则跟着
  格式走，领域词汇保持格式无关。
- **两阶段可中断**：等待模型消息与等待工具执行都能被打断，且都会以
  `turn_end` 收尾——工具阶段必须补齐每个调用的结果。打断/中止会丢弃
  队列里尚未执行的模型调用/工具派发，避免收场后继续执行。
- **内容过滤是一等结果**：模型返回 `content_filter` 时，本轮以
  `Outcome::ContentFiltered` 收场；已产生的文本仍作为普通 assistant 消息入账。

## 开发

```bash
cd agnes-rs
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

工作区统一启用 `unsafe_code = "forbid"`，各 crate 通过
`[lints] workspace = true` 继承。

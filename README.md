# agnes

终端 AI 编程助手的 Rust 实现，兼容任意 OpenAI 风格的 Chat Completions API。

agnes 采用六边形架构：纯领域核心（类型状态机）与所有 I/O 适配器彻底分离，
模型协议、HTTP 传输与工具均可独立替换。它提供 CLI 二进制 `agnes`，
既能执行一次性任务，也能进入带完整会话记忆的交互式会话。
项目处于早期开发阶段（0.1.0）。

## 特性

- 类型状态机驱动的 agent 循环，非法协议顺序在编译期即不可构造
- 一轮 turn 的生命周期（`turn_start` / `message_*` / `tool_execution_*` /
  `turn_end`）由系统管线按序铸造并记入 `Log`；core 只持有领域状态
  （`Turn`），消息、系统提示与工具环境都可从日志复原，一次请求也能从
  日志单独推导
- 插件即装配单元：HTTP 传输、模型 provider、会话持久化、工具、权限与
  审批都是 ecs `Plugin`，注册顺序即每 tick 的系统执行顺序；拒绝单调
  （规则无法把拒绝变回放行），运行中输入排队，在 step 边界生效
- 工具调用可走审批流：`allowed-once | rejected | cancelled | unavailable`，
  没接审批口即拒绝（fail-closed）
- 会话日志可落盘：JSONL、单写者、崩溃后补写收尾（绝不重写已有记录）；
  审批决定与失败原因作为宿主审计记录，与事件共享同一顺序
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
./target/release/agnes run "解释 core/src/agent.rs 中的状态机"
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
端口，`ecs` 提供系统编排，其余 crate 是适配器、插件与宿主。

| crate | 职责 |
| --- | --- |
| `agnes-util` | 零依赖基础类型：`NonEmpty<T>`、`EmptyError` |
| `agnes-core` | 纯领域模型与类型状态机（`Agent` + `Turn`），无 I/O、无异步、不含序列化、不产出事件 |
| `agnes-ecs` | 独立的轻量系统编排运行时：资源、实体、消息总线、事件循环（`App` + `Plugin`），不含具体异步运行时 |
| `agnes-record` | 记录词汇与会话状态：`Event` / `HostEvent` / `RunEvent`、`Log`、`Durability`，以及 `SessionLog` / `SessionStore` 端口（零 ecs 依赖） |
| `agnes-io` | 异步端口 trait（`Tool`、`HttpTransport`）与模型调用词汇（`LlmCall` / `LlmReply` / `LlmCancel` / `LlmProvider`） |
| `agnes-llm-openai` | OpenAI Chat Completions 纯编解码器（`OpenAiCodec`：请求编码、响应解码，无 I/O） |
| `agnes-transport-reqwest` | 基于 `reqwest` 的 HTTP 传输适配器 |
| `agnes-session-file` | 会话日志的 JSONL 实现：单写者、格式版本、崩溃补写；线格式（wire）定义在此 |
| `agnes-plugin-async` | 异步任务执行：`TokioPlugin` / `ExecutorPlugin` 两个插件各自装 `AsyncExecutor`，任务直接交给具体运行时 |
| `agnes-plugin-http` | ECS 原生 HTTP 能力：`HttpRequest` / `HttpResponse` / `HttpCancel` 消息、发号器 `HttpClient`、在飞表与取消、关停时统一取消 |
| `agnes-plugin-openai` | provider 桥：`LlmCall` 经 `OpenAiCodec` 编码成 `HttpRequest`，`HttpResponse` 解码回 `LlmReply`；装 `LlmProvider` 标记 |
| `agnes-plugin-session` | 会话状态与策略：`Log`、`Durability`、序号、`RunEvent` 转发、flush 确认、轮转；不碰文件 |
| `agnes-plugin-session-file` | 会话持久化：把打开的 `SessionFile` 装成 `SessionStoreResource`，拥有文件句柄、兄弟文件名与重开 |
| `agnes-agent` | `AgentPlugin`：消息管线驱动 typestate；工具注册、权限规则与审批通道 |
| `agnes-cli` | 二进制 `agnes`：装配 `App`、stdin/审批/Ctrl-C 宿主任务、独立写线程的终端渲染 |

`agnes-transport-reqwest`、`agnes-llm-openai` 与 `agnes-session-file` 是纯
适配器；`plugin-async` / `plugin-http` / `plugin-openai` / `plugin-session` /
`plugin-session-file` 把能力装进 ecs 世界，`record` 持有词汇、`agent`
持有管线，互不知晓对方。只有 `agnes-cli` 负责把具体插件组装成可运行的
程序；插件注册顺序即行为：`HttpPlugin` 必须先于 `OpenAiPlugin`（provider
桥 build 时检查 `HttpClient`），`SessionPlugin` 先于 `AgentPlugin`
（落盘确认要在同一 tick 内先于耐久门禁与收场报告可见）；`TokioPlugin`
只需在 `run_async` 之前加入（资源在每 tick 解析）。

模型调用是一条纯消息链：运行时写 `LlmCall` → provider 桥编码成
`HttpRequest` → HTTP 能力执行并回 `HttpResponse` → 桥解码成 `LlmReply` →
运行时推进状态机。每跳一条消息即一个 tick；`drain` 会连续推进相邻 tick，
所以可感延迟趋近于零，而运行时与网络彻底解耦（打断经 `LlmCancel` /
`HttpCancel` 取消请求，取消总会闭环成一条 `LlmReply`）。

### 关键设计

- **类型状态机**：`Agent<Ready>` / `Agent<ReadyWithGoal>` →
  `Agent<AwaitingCall>` → `Agent<AwaitingModel>` → `Agent<AwaitingTools>` →
  `Agent<Closed>`；转移消耗当前状态、交还下一个阶段（或携带下一阶段的
  枚举），阶段本身就是动作提示。就绪阶段可以携带 goal（`start` 时追加进
  `Turn`），也可以不带 goal 直接基于已有历史启动；只有待调用阶段可以
  注入消息，`call` 才冻结请求、开启 step 并进入等响应阶段。`Agent` 持有
  一个 `Turn`（系统提示与工具环境、会话消息、用量、step 记账）；core
  不持有日志、不产出事件，`agnes-agent` 的系统管线读取 `Turn` 并铸造、
  记录事件。
- **插件与权限**：每个能力是一个 ecs `Plugin`，注册顺序即每 tick 的系统
  执行顺序；插件之间只通过资源（`HttpClient`、`HttpTransportResource`、
  `SessionStoreResource`、`AsyncExecutor`、`ToolRegistry`、
  `PermissionRules`、`ApprovalChannel`、`Durability`、`LlmProvider`）与
  消息协作。权限规则 deny > ask > 不表态，拒绝单调（规则无法把拒绝变回
  放行，批准后还会做第二次门禁）；ask 命中而没接审批口时按 fail-closed
  拒绝，`--yes` 自动批准但审计照记。
- **启动即校验**：`ask` 规则命中却无从应答（管道输入、无 TTY 又没给
  `--yes`）时启动即报错，而不是挂起等待；配置错误不等到第一次调用。
- **端口与适配器**：`Tool`、`HttpTransport` 是 `io` 的 `async_trait`
  端口；`SessionLog` / `SessionStore` 随记录词汇归 `agnes-record`。
  `OpenAiCodec` 是纯编解码，不绑定任何传输。
- **非法状态不可表示**：`StopReason::ToolUse` 携带非空的
  `NonEmpty<ToolCall>`，`Content` 保证非空，`ApiKey` 由 `secrecy`
  承载，`ApprovalOutcome` 只有 `AllowedOnce` 会放行。
- **工具失败即输出**：工具错误以 `ToolOutput` 形式回传给模型（拒绝、
  挡回、超时同样如此）；只有模型或传输错误会中止运行。
- **日志即账本**：`Log` 类型位于 `agnes-record`，由系统管线持有，是唯一
  的持久化状态；core 只持有领域状态 `Turn`，不产出事件。系统提示与工具
  环境是 `Turn` 的 header 字段；系统管线在每个 turn 开局记录一条
  `record::Event::Header`，工具环境运行中变化时再记一条。消息也是事件
  （`record::Event::Message`），用户输入与注入都是带角色的消息事件。
  `Turn::prompt()` 把 header、消息与工具表投影成模型输入，因此一次请求
  完全可以从日志复原；带 `--session` 时，恢复出的事件直接重建 `Log`，
  下一轮从日志继续。
  工具集变化在下一个模型调用前记一条新 header，
  同一 turn 内生效；在飞调用不受影响。
- **日志耐久**：每个上报事件先入内存日志、再作为 `RunEvent` 镜像发出；
  `agnes-plugin-session` 把会话相关的事件按序转发给常驻 writer 任务（经
  `SessionStore` 端口；`agnes-plugin-session-file` 提供文件实现，无 store
  时为内存模式、序号随日志同步前移）
  （每批 append 后 `flush` + fsync，经 `SessionFlushed` 消息回执），并
  维护 `Durability { sent_seq, flushed_seq }`。**耐久门禁**是提交边界：
  `flushed_seq >= sent_seq` 之前不发出模型请求 —— 请求离开前，已记录的
  日志必然已经落盘。格式 v6：首行版本标记，之后是 `record`
  事件行与 `host` 审计行（审批问询/决定、失败原因），共享一个
  连续 `seq`；恢复时校验连续性并**补写**未收尾的 step/turn（与
  `Agent::interrupted` 同形），从不重写已有记录；旧格式（含 v5）与重名
  工具环境拒载。`seq` 只在实际写成功后提交，写失败会 poison 会话句柄，
  避免半行之后继续追加；会话写失败是 fail-closed 的 —— 活跃 turn 以
  失败收场，不再发起新调用。v6 里 `MessageEnd` 只记无载荷的 `StopKind`，
  `StepOutcome`/`Outcome` 不再复制答复文本。
- **审计与可追溯**：`MessageEnd` 记录产生该消息的模型（从系统自己构造的
  `LlmRequest.model` 回带），切换模型后日志仍能归因；审批落定、会话失败
  原因不扩 core 词汇，作为 `HostEvent` 与事件一起落盘。
- **转换者由宿主决定**：core 不组装提示词；系统管线在每次模型调用前用
  宿主侧的 `RequestBuilder`（`agnes-agent`）把 `Turn` 转成 `LlmRequest`。
- **格式由上层定义**：core 与 record 的领域词汇（`Turn`、`Event`、
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

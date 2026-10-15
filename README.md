# agnes

终端 AI 编程助手的 Rust 实现，兼容任意 OpenAI 风格的 Chat Completions API。

agnes 采用六边形架构：纯领域核心（类型状态机）与所有 I/O 适配器彻底分离，
模型协议、HTTP 传输与工具均可独立替换。它提供 CLI 二进制 `agnes`，
既能执行一次性任务，也能进入带完整会话记忆的交互式会话。
项目处于早期开发阶段（0.1.0）。

## 特性

- 类型状态机驱动的 agent 循环，非法协议顺序在编译期即不可构造
- 端口与适配器分离：`core` 无 I/O、无异步，替换运行时只需更换适配器
- 边界处一次性解析：模型名、URL、API key、工具名等都是强类型
- 秘密保护：API key 由 `secrecy` 承载，调试输出不泄露明文
- 内置沙箱化的文件工具，以及默认关闭的 shell 工具
- 运行中可中断、可注入用户消息，turn 预算防止失控循环

## 快速开始

需要 Rust 1.85+（edition 2024）。

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

- `/new`：清空此前对话（保留系统提示），开始全新会话
- `exit`、`quit` 或 EOF：退出会话
- `Ctrl-C`：任务运行中中断任务并保留会话；在提示符处则退出
- 运行期间输入的消息会排队，在下一批工具调用结束后注入

### 命令行选项

以下选项均为全局选项，对子命令同样生效：

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `--model <MODEL>` | `gpt-4o-mini` | 模型名称 |
| `--base-url <URL>` | `https://api.openai.com/v1` | OpenAI 兼容 API 地址 |
| `--api-key <KEY>` | 见环境变量 | API 密钥 |
| `--max-turns <N>` | `32` | 每个任务的最大模型轮次 |
| `--allow-shell` | 关闭 | 向模型提供 `run_shell` 工具 |

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
单次工具输出超过 100 000 字符时截断。

## 架构

工作区位于 `agnes-rs/`，按依赖方向分层：`core` 是纯计算，`io` 定义
端口，其余 crate 是适配器与宿主。

依赖方向（箭头指向被依赖的 crate）：

```text
util ← core ← io ← agent ← cli
```

`agnes-transport-reqwest` 与 `agnes-llm-openai` 是挂在 `io` 之上的
适配器；只有 `agnes-cli` 负责把具体实现组装成可运行的程序。

| crate | 职责 |
| --- | --- |
| `agnes-util` | 零依赖基础类型：`NonEmpty<T>`、`EmptyError` |
| `agnes-core` | 纯领域模型与类型状态机，无 I/O、无异步 |
| `agnes-io` | 异步端口 trait：`Llm`、`Tool`、`HttpTransport`、`Input` |
| `agnes-llm-openai` | OpenAI Chat Completions 协议适配，泛型于传输层 |
| `agnes-transport-reqwest` | 基于 `reqwest` 的 HTTP 传输适配器 |
| `agnes-agent` | 异步驱动器、事件流与内置工具 |
| `agnes-cli` | 二进制 `agnes`：参数解析、REPL 与会话管理 |

### 关键设计

- **类型状态机**：`Agent<Ready>` → `Agent<AwaitingModel>` →
  `Agent<AwaitingTools>` → `Agent<Finished>`。每次转移消耗当前状态并
  返回下一个状态与效果（`ModelTurn` / `ToolsTurn`）；上层驱动器解释
  效果、组装 `LlmRequest` 并调用模型。
- **端口与适配器**：`Llm`、`Tool`、`HttpTransport`、`Input` 都是
  `async_trait` 端口；`OpenAiClient<T: HttpTransport>` 不绑定具体传输。
- **非法状态不可表示**：`StopReason::ToolUse` 携带非空的
  `NonEmpty<ToolCall>`，`Content` 保证非空，`ApiKey` 由 `secrecy`
  承载，错误类型不携带字符串载荷。
- **工具失败即输出**：工具错误以 `ToolOutput` 形式回传给模型；
  只有模型或传输错误会中止运行。
- **会话记忆**：`Context` 保存系统提示与历史消息，
  `RunSummary.context` 可直接传回下一次运行；`/new` 调用
  `Context::reset()`。

## 开发

```bash
cd agnes-rs
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

工作区统一启用 `unsafe_code = "forbid"`，各 crate 通过
`[lints] workspace = true` 继承。

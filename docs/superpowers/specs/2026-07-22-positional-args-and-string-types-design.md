# agnes 位置参数与 String 类型简化 - 设计规格

> 日期：2026-07-22
> 项目：agnes（Rust edition 2024，Cargo workspace）
> 类型：语言语法 + 类型系统简化
> 状态：待实现

---

## Context（为什么做这件事）

在 chat agent loop 中让 agnes "写一个简单的 Rust web 服务器到文件"，LLM 连续两轮生成的 DSL 都失败，暴露了当前设计的两个根本问题：

**问题 1 - 类型不可达。** 字符串字面量是 `String`，但文本类工具要求 `PlainText` / `Markdown` / `TextLike`：
```lisp
(tool join-lines :lines ["use std::net::TcpListener;" "fn main() { ... }"])
```
`join-lines` 要求 `:lines` 为 `List (| PlainText Markdown)`，而字面量列表是 `List String`。类型系统无子类型、无 coercion（`type_expr_matches` 只做精确名匹配 + union 成员 + `Unknown` 通配），`String` 与 `PlainText` 完全不相交。LLM **无法用字面量构造出 `PlainText` 这类值**——它们只能作为 `read-file` / `translate` / `llm` 的输出产生。`Markdown` / `HTML` / `Summary` / `PDF` / `Image` 甚至连产出方都没有，是输入侧完全不可达的死类型。

**问题 2 - 关键字参数语法对 LLM 不友好。** 工具调用用 `(tool name :k v ...)` 关键字参数。LLM 想让管道上游填充某个参数时，写出了 `(tool write-file :path "server.rs" :content)` —— 裸 `:content` 后面没有值，触发 parse error `keyword :content without value`。`:name arg` 这种形式原本期望帮助 LLM 生成正确代码，实测反而成为坑。

本设计针对这两个问题做"外科式"简化：移除不可达的语义文本/文档类型（文本统一为 `String`），并把工具调用改为原始 Lisp 风格的位置参数。

## Goals

- 字符串字面量能直接喂给文本类工具（`join-lines` / `write-file` 的 `:content` 等），不再类型报错。
- 工具调用语法对 LLM 更自然：位置参数，无 `:k v`。
- 保留 agnes 已验证有价值的能力：Path / JSON 的运行时校验安全特性、`define` 的类型标注、DAG 编译与 agent loop 语义。
- 顺带去掉 `llm` 的冗余特殊化（`Expr::Llm` / `NodeKind::Llm` 与普通工具重复）。

## Non-goals

- 不改 `define` / `declare tool` 的 `:params` / `:provides` 语法（那是特殊形式语法，不是工具调用的 `:name arg`）。
- 不引入显式管道占位符（如 `?`）；管道绑定沿用既有"唯一未填参数"规则。
- 不重做 `Path` / `JSON` 校验。
- 不恢复 `ocr` / PDF / Image 能力（见下，连类型一起删）。

---

## §1 类型系统

### 移除的类型

`PlainText`、`Markdown`、`HTML`、`Summary`、`PDF`、`Image`，以及别名 `TextLike`（`| PlainText Markdown HTML`）、`VisualDoc`（`| PDF Image`）。

随同移除的校验器与 show 实现：
- `types::utf8_validator`（原 PlainText/Markdown/HTML/Summary 共用，移除后无使用者）
- `types::pdf_validator`、`types::image_validator`
- `shows` 中 `plain_text` / `markdown` / `html` / `summary` / `pdf` / `image` 六个 show 函数及 `BUILTIN_SHOWS` 对应条目

### 保留的类型

`Path`、`JSON`、`Unit`、`Unknown`、`String`、`Int`、`Bool`、`CommandResult`、`Finish`、`Observation`。

`Path` / `JSON` 保留真实校验器（`path_validator` / `json_validator`），其安全价值不变。

### 移除的工具

`ocr` —— 它是 `PDF` / `Image`（`VisualDoc`）的唯一消费者，类型删除后无法再定义合法输入，整工具删除（签名 + 实现 + show 无关）。

### 工具签名（改后）

| 工具 | requires | provides |
|---|---|---|
| read-file | `path: Path` | `String` |
| write-file | `path: Path`, `content: String` | `Unit` |
| summarize | `input: String` | `String` |
| translate | `input: String`, `lang: String` | `String` |
| llm | `prompt: String`, `input: String` | `String` |
| join-lines | `lines: List String` | `String` |
| shell-run | `command: String` | `CommandResult` |
| parse-path | `path: String` | `Path` |

> 注：`parse-path` 当前只有实现、**未注册签名**（既有缺口，checker 会把它当 `UnknownTool` 拒收）。本次顺带补注册为 `path: String -> Path`，使签名表完整一致。

要点：
- 所有原 `provides PlainText` 的工具改为 `provides String`。
- 文本类 `requires` 参数（`content` / `input` / `lines`）改为 `String`（`join-lines` 的 `List String`）。
- 参数**名**不变（`lines` / `content` / `input` / `path` / `prompt` / `lang` / `source`→删 / `command`），因此工具实现层按名取值的代码零改动。
- `summarize` 的 input 简化为纯 `String`（原 `(| PlainText Markdown HTML PDF)` 中所有成员都已删除）。

### 关键效果

`(tool join-lines ["a" "b"])` 合法——字符串字面量是 `String`，匹配 `List String`。

---

## §2 语法

### 工具调用

```lisp
(tool name arg1 arg2 ...)   ; 纯位置参数，无 :k v
```

参数顺序由工具签名的 `requires` Vec 顺序定义，作为用户契约。例如 `(tool write-file "server.rs" content)` 中 `path` 在前、`content` 在后。

### 管道绑定

不变，沿用"唯一未填必需参数自动绑定上游"规则（checker `check_tool_call` 与 runtime `bind_tool_args` 已一致实现）：

```lisp
(pipe
  (tool join-lines ["use std::net::TcpListener;" "fn main() { ... }"])
  (tool write-file "server.rs")        ; content 未填 -> 绑上游
  (finish "已写入 server.rs"))
```

> `(pipe "done" finish)` 这种 pipe 内裸符号 desugar 不变（产出 `Expr::Tool { positional: vec![] }`，零位置参数，上游绑到唯一未填参数）。

### 移除项

- `Expr::Llm` —— `llm` 降级为普通工具，调用形如 `(tool llm "prompt" "input")`。无管道时 `input` 显式传 `""`（与现状 `(tool llm :prompt "say hi" :input "")` 等价；原 checker 本就要求 `input` 填充或管道绑定）。
- `NodeKind::Llm` —— lower 为 `NodeKind::Tool { name: "llm" }`。`NodeKind::Llm` 与 `NodeKind::Tool` 在 scheduler 中实现几乎完全相同（均 `collect_kwargs` + `call_native_traced`，唯一差别是 Llm 硬编码名 `"llm"`），删除无语义损失。
- `Expr::Tool.args`（`KwArgs` 字段）。`KwArgs` 类型别名只被 `Expr::Tool` 与 `Expr::Llm` 使用（`retry` / `catch` 用各自专用字段 `times` / `fallback`，不走 `KwArgs`），两者移除后若全局无其他使用者，删除 `pub type KwArgs`。

### 保留项（特殊形式，不受影响）

`define`（`:params` / `:provides`）、`retry`（`:times`）、`catch`（`:fallback`）、`if` / `match` / `foreach` / `pipe` / `par` / `let` / `return` / `finish` / `observe` / `list`。这些是特殊形式的自有语法，不是工具调用的 `:name arg`，保留。

---

## §3 实现层改动

数据流端到端位置参数化（parser → AST → checker → DAG → runtime），但工具实现层不变：工具闭包仍按参数名从 `HashMap<String, Value>` 取值，名字不变，只是来源改成按位置映射、类型校验放宽。

| crate | 改动 |
|---|---|
| agnes-ast | `Expr::Tool` 去掉 `args: KwArgs`（仅剩 `span` / `name` / `positional`）；删除 `Expr::Llm` 变体；`KwArgs` 别名若无其他使用者则删 |
| agnes-parser | `parse_tool` 改为只解析位置参数（去掉 `parse_positional_and_kwargs` 的 kwarg 部分，该函数若不再有调用方则删）；`parse_expr` 删 `"llm" => Expr::Llm` 分支；pipe 裸符号 desugar 不变 |
| agnes-checker | `check_tool_call` 去掉 kwarg 迭代块，保留位置参数 + "唯一未填绑定上游"；删 `check_expr` 的 `Expr::Llm` 分支（`llm` 走 `check_tool_call`） |
| agnes-compiler | `lower.rs`：`Expr::Tool` 去掉 kwarg lowering，`llm` lower 为 `NodeKind::Tool { name: "llm" }`；`dag.rs`：删 `NodeKind::Llm` 变体及其构造/匹配；`cycle.rs` 核对无 Llm 引用 |
| agnes-runtime | `bind_tool_args` 去掉 kwarg 迭代；删 `eval_expr` 的 `Expr::Llm` 分支（含原 `:input` 特殊绑定逻辑）；删 `eval_node` 的 `NodeKind::Llm` 分支；`collect_kwargs` 仍按位置 inputs 映射到参数名 HashMap（工具实现按名取值，不变） |
| agnes-builtins | `lib.rs`：删 `register_type` 的 PlainText/Markdown/HTML/Summary/PDF/Image、`TextLike`/`VisualDoc` 别名、`ocr` 工具签名，按 §1 表更新其余签名，**补注册 `parse-path`**；`aliases.rs`：删 `text_like()` 与 `visual_doc()`（模块若空则删）；`types.rs`：删 `utf8_validator` / `pdf_validator` / `image_validator`；`shows.rs`：删 6 个 show 函数及条目；`tools.rs`：删 `ocr` 实现，其余工具实现零改动 |
| agnes-llm | `planner.rs`：从向 LLM 广告的工具列表中移除 `ocr`；系统提示词见 §4 |

---

## §4 迁移

### Planner 提示词（agnes-llm/src/planner.rs）

- 工具调用语法改为 `(tool name arg1 arg2 ...)` 位置参数，去掉所有 `:k v` 示例。
- 类型示例去掉 `PlainText`，文本统一 `String`；`define` 的 `:provides` 用 `String`。
- 说明管道绑定规则：要上游填充某参数时**省略该参数**，不要写裸 `:name`。
- 移除 `ocr` 相关示例。

### 示例与文档

- `examples/`：`full-demo.agnes` / `translate.agnes` / `hello.agnes` / `with-define.agnes` / `fan-out.agnes` 全部转位置参数，`:provides PlainText` → `:provides String`。
- `README.md` "Language at a glance" 示例同步。
- `examples/chat-demo.md` 同步。

### 测试

跨 crate 更新/删除：

- `agnes-parser/tests/parse.rs`：删 `(declare type PDF)` 与 `(declare tool ocr :requires [(source (| PDF Image))])` 相关用例（或改用其他类型名）。
- `agnes-builtins/tests/`：`shows.rs` 删 PDF/Image show 测试；`register.rs` 删 ocr 签名测试；`dispatch_routing.rs` 删 ocr dispatch 测试；`finish_observe.rs` 已无 PlainText 引用（q 版），核对。
- `agnes-checker/tests/check.rs`：删本地 `register_type PDF/Image` 与 ocr 用例（含 `(pipe (tool read-file ...) (tool ocr))`）。
- `agnes-llm/tests/planner.rs`：删 `ocr` 引用。
- `agnes-registry/tests/register.rs` 与 `agnes-types/src/lib.rs` 测试：其中 `"PDF"` 仅作任意类型名 fixture（自包含注册，不依赖 builtins），保留即可；如为整洁可改名，非必须。
- 其余凡用 `:k v` 工具调用或 `PlainText` 类型的测试一律转位置参数 + `String`。

实现计划阶段会逐一枚举每个测试文件的具体改动。

---

## §5 明确不做

- `define` / `declare tool` 的 `:params` / `:provides` 不动。
- Path / JSON 校验保留。
- 不引入 `?` 占位符（仅省略规则）。
- 不恢复 ocr / PDF / Image。

---

## §6 验证

1. `cargo build --workspace` 通过。
2. `cargo test --workspace` 全绿。
3. **端到端**：在 chat agent loop 中重放"写一个简单 Rust web 服务器到 server.rs"，LLM 生成的 `(pipe (tool join-lines [...]) (tool write-file "server.rs") (finish ...))` 能成功执行，`server.rs` 落盘且内容正确——即原始两个失败场景都被修复。
4. 既有示例（`cargo run -p agnes-cli -- examples/full-demo.agnes` 等）仍可运行。

---

## 影响范围摘要

- 删除：6 个类型、2 个别名、3 个校验器、6 个 show、1 个工具（ocr）、2 个 AST 变体/字段（`Expr::Llm`、`Expr::Tool.args`、`NodeKind::Llm`）、`KwArgs`（条件）。
- 修改：8 个工具的签名、parser/checker/compiler/runtime 的工具调用与 llm 处理路径、planner 提示词、5 个示例 + README + chat-demo、跨 crate 测试。
- 保留：Path/JSON 校验、define 语法、DAG 编译、agent loop 语义、finish/observe special form。

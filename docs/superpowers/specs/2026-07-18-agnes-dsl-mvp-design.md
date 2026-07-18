# agnes DSL MVP — 设计规格

> 日期：2026-07-18
> 项目：agnes（Rust edition 2024，Cargo workspace）
> 类型：新语言 + Runtime 设计
> MVP 目标：**语言可行性（Language Feasibility）** —— 证明 "Lisp + Trait" 是好的 agent workflow 编排抽象

---

## Context（为什么做这件事）

目标是开发一个新的 agent，使用一门专用的 workflow DSL 来编排任务，最终愿景是"达到 JavaScript 之于浏览器的效果" —— 成为 agent 编排的 canonical 语言 + runtime。

现有 agent 编排方案（LangGraph / Dify / n8n / 直接 LLM 调 tool）存在以下问题：

- 编排逻辑要么藏在代码里（不可移植），要么藏在图形拖拽里（LLM 无法自动生成）
- Tool 调用没有 **semantic type**，LLM 组合 tool 时全靠 prompt 里的自然语言描述，容易出错
- 缺少能被静态检查、能被优化、能被复用的**中间表示（Intermediate Representation）**

**agnes 的定位：** 一门 LLM 友好的、可编译为 DAG 的、带 semantic trait 类型系统的 workflow DSL + runtime。LLM 只负责**规划（生成 DSL）**，runtime 负责**执行**。

**MVP 的验证目标：** 用 5 个内置 tool，写出一个通过类型检查、包含 `pipe / par / let / define / declare` 全部核心特性的 workflow，跑通端到端。证明这门语言的**抽象是对的** —— 不追求性能、生态、工具丰富度。

---

## 关键设计原则（Pin 到文件头）

1. **DSL 描述逻辑，Runtime 负责执行，MCP 负责连接工具，Trait 负责工具组合，LLM 负责规划**
2. **LLM 是一等公民使用者** —— 语法机械化、错误信息即修复模板
3. **显式 > 隐式**，除数据流外的所有语义都显式表达
4. **DAG 保证无环**，任何递归 = 编译错误
5. **默认 fail-fast**，重试/容错必须显式
6. **Trait 表达能力，Type 表达事物** —— Trait 用形容词/动名词（如 `Readable`, `OCRable`），Type 用名词（如 `PDF`, `Markdown`）；Trait 与 Type **共享命名空间**，禁止重名

---

## 一、核心语言特性（全景）

### 1.1 顶层指令（Top-level Directives）

顶层指令注册 registry 条目，**不参与执行流**。

| 指令 | 作用 |
|---|---|
| `declare trait` | 声明一个 semantic trait（可选，仅用于文档） |
| `declare type` | 声明一个 type 并列出它实现的 trait |
| `declare tool` | 声明或覆盖一个 tool 的类型签名 |
| `define` | 定义一个复合 tool（workflow-as-tool） |

### 1.2 表达式指令（Expression Forms）

只在 workflow body 中出现。

| 指令 | 作用 |
|---|---|
| `tool` | 调用一个 tool |
| `pipe` | 串行流水线（隐式数据流） |
| `par` | 并行执行 |
| `let` | 命名绑定（透明/侧线两形态） |
| `if` | 二分支条件 |
| `match` | 多分支条件 |
| `foreach` | 遍历集合 |
| `retry` | 重试包装 |
| `catch` | 错误捕获 + fallback |
| `llm` | 调用 LLM（内置特殊 tool） |
| `return` | 显式返回值 |

### 1.3 语法风格（S-expression + keyword args）

```lisp
;; 顶层
(declare type PDF
  :implements [Binary OCRable])

(declare tool ocr
  :requires [(source: OCRable + Binary)]
  :provides Text)

(define pdf-to-summary
  :params  [(file: PDF)]
  :provides Summary
  (pipe
    (tool ocr :source file)
    (tool summarize)))

;; 表达式
(pipe
  (tool read-file :path "x.pdf")
  (let doc)                              ; 透明命名
  (par
    (let sum (tool summarize))           ; 侧线绑定
    (let ja  (tool translate :lang "ja")))
  (tool merge :summary sum :translation ja))
```

---

## 二、类型系统（Type System）

### 2.1 两层模型

| 层 | 载体 | 用途 |
|---|---|---|
| **Transport Type** | JSON Schema | 序列化与工具调用参数 |
| **Semantic Type + Trait** | agnes registry | Workflow 编译期类型推导 |

Transport Type **不参与** workflow 类型检查。所有 semantic 层面的判定都基于 type 及其实现的 trait 集合。

### 2.2 概念（Rust 风格）

| 概念 | 说明 | 命名风格 | 举例 |
|---|---|---|---|
| **Trait** | 语义能力/契约，无载体 | 形容词 / -able / -ing | `Readable`, `OCRable`, `Translatable`, `Summarizable` |
| **Type** | 具体数据类型，实现若干 trait | 名词 | `PDF`, `Markdown`, `Image`, `Text`, `Summary` |
| **Tool 签名** | `requires`（trait 组合）→ `provides`（type 或 trait） | | `ocr: OCRable + Binary → Text` |

**关键规则：Trait 和 Type 共享命名空间，禁止重名。** 注册冲突时编译器立刻报错，避免语义混淆。

一个 type 可实现任意多个 trait（`String: Display + Debug` 的语义类比），`+` 表示 trait 交集（AND）：

```lisp
(declare type PDF      :implements [Binary OCRable])
(declare type Markdown :implements [Readable Writable Translatable Summarizable])
(declare type HTML     :implements [Readable Writable Translatable Summarizable])
```

**Tool `provides` 的两种形态：**
- Provides a **type**（推荐）：`:provides Text` —— tool 承诺产出的具体事物
- Provides trait(s)：`:provides [Readable Writable]` —— 只承诺能力，具体 type 不定（用于复合 tool 或多态场景）

### 2.3 类型检查规则（就三条）

1. **参数满足（Parameter Satisfaction）**：调用点每个参数的 type 所实现的 trait 集合必须 ⊇ 该 tool 参数声明的 trait 集合
2. **流向满足（Flow Satisfaction）**：`pipe` 中前一步的 provides trait 集合必须 ⊇ 后一步的 requires trait 集合（仅当后一步单参数时启用隐式流）
3. **复合 tool 签名一致（Compound Signature Consistency）**：`define` 的 body 最终 provides 必须 ⊇ `:provides` 声明

判定基于 `HashSet<TraitName>` 的 `is_superset`。当 provides 是一个 type 时，参与检查的是该 type 实现的完整 trait 集合。

### 2.4 LLM 友好的错误信息模板

**关键设计决策：** 编译错误信息本身就是给 LLM 的修复模板。每条错误必须包含 **What / Why / Fix suggestion** 三段：

```
Type error at (tool ocr :source md-file):
  parameter `source` requires trait(s): OCRable + Binary
  but `md-file` has type: Markdown
  Markdown implements: Readable + Writable + Translatable + Summarizable
  missing traits: OCRable, Binary

Fix suggestion (one of):
  A) Change upstream to produce a type implementing OCRable + Binary (e.g., PDF, Image)
  B) Extend Markdown's trait set (paste at top of file):
     (declare type Markdown :implements [Readable Writable Translatable Summarizable OCRable Binary])
```

命名空间冲突错误：

```
Name conflict: `Readable` is already registered as a trait
  attempted: (declare type Readable ...)
  suggestion: rename to `ReadableDoc`, `TextDoc`, or similar noun form
```

`declare` 的**主要使用者是 LLM 本身** —— 语法必须机械化、位置固定、可直接从错误信息生成。

---

## 三、DSL 语义细节（关键规则）

### 3.1 数据流

- `pipe` 内相邻表达式之间存在隐式流：前一步的 provides 作为后一步的**唯一位置参数**注入
- 多输入 tool：显式用 `:key value` 传参，`value` 可以是字面量或已绑定的名字
- `(let name)` 单参数形态：给"当前流"起名，**流本身继续向下**
- `(let name expr)` 双参数形态：求值 expr 并绑定，**不进入 pipe 流**（侧线计算）
- `par` 内部的 `(let name ...)` 绑定作用域是**外层 pipe**（`let*` 语义）

### 3.2 控制流

- `pipe` / `par` / `if` / `match` / `foreach` 都是表达式，可嵌套，都有返回值
- `par` 的返回值是各分支的 tuple（顺序对应书写顺序）；若分支内用了 `let`，那些名字提升到外层
- `if` 两分支类型必须兼容（trait 交集非空）；`match` 同理

### 3.3 错误处理（分层组合式，默认 fail-fast）

**默认行为：** 所有 tool 错误立即向上传播，workflow 停止。
Registry 层默认 `:retry 0 :on-error propagate`。

两种等价语法：

```lisp
;; 控制流形式：包一段 pipeline
(catch :on TimeoutError :fallback (tool return-default)
  (retry :times 3
    (pipe
      (tool fetch-url :url "...")
      (tool parse-response))))

;; 修饰符形式（语法糖）：包单个 tool
(pipe
  (tool fetch-url :url "..." :retry 3)
  (tool parse-response))
```

编译器把修饰符形式**降级（desugar）**为控制流形式，DAG 只认一种 IR。

### 3.4 `define` 语义

- 显式声明 `:params` 和 `:provides`
- 支持参数默认值：`(doc: Readable :default nil)`
- **MVP 不支持递归** —— 编译期做拓扑排序检测环
- 一个 `define` 注册后，与内置 tool **调用方式完全一致**

### 3.5 `declare` 的三种形态

```lisp
(declare trait <Name> [:doc "..."])           ; 可选，仅文档
(declare type  <Name> :implements [<Trait>...])
(declare tool  <Name>
  :requires [(<param>: <Trait> + <Trait>) ...]
  :provides <Type> | [<Trait>...])
```

- 顶层 `declare` 覆盖或补全 Tool Registry
- 未知 MCP tool 的初始 type 为 `Unknown`（不实现任何 trait），必须通过 `declare` 补全后才能进入 workflow
- Runtime 缓存推断结果（LLM 推断的 declare 片段可持久化）
- **Trait / Type 命名空间冲突强制报错**

---

## 四、Runtime 架构

```
User
 │
LLM Planner              ← 生成 agnes DSL
 │
agnes source (.agnes)
 │
agnes-parser             → S-expr AST
 │
agnes-registry           ← declare / define 注册（含命名空间冲突检查）
 │
agnes-checker            → trait superset 判定
 │
agnes-compiler           → AST + Registry → DAG（含环检测）
 │
agnes-runtime            → 按 DAG 调度，处理 retry / catch
 │
Tool Provider Layer      → 调用 tool
   ├── Native (agnes-builtins)          ← MVP 只做这一个
   ├── MCP servers                       ← 后续阶段
   ├── CLI tools                         ← 后续阶段
   └── HTTP endpoints                    ← 后续阶段
```

---

## 五、Rust 实现：Cargo Workspace 结构

**根目录** `/home/hao/code/agnes/` 是 workspace，代码在 `crates/` 下。

```
agnes/
├── Cargo.toml                              ← workspace 根（[workspace] members = ...）
├── crates/
│   ├── agnes-ast/                          ← AST 定义（叶子 crate，无内部依赖）
│   ├── agnes-parser/                       ← S-expr → AST（依赖：agnes-ast）
│   ├── agnes-types/                        ← TraitName / TypeName / ToolSignature
│   ├── agnes-registry/                     ← Trait/Type/Tool 注册表 + 命名空间冲突检查
│   ├── agnes-checker/                      ← 类型检查器 + 错误模板
│   ├── agnes-compiler/                     ← AST → DAG（define 展开、环检测）
│   ├── agnes-runtime/                      ← DAG 执行器（tokio async）
│   ├── agnes-builtins/                     ← 5 个内置 tool 的 Rust 实现
│   └── agnes-cli/                          ← 命令行入口（binary）
├── examples/                               ← .agnes 示例文件（workspace 级）
├── tests/                                  ← workspace 级集成测试（可选）
└── docs/superpowers/specs/2026-07-18-agnes-dsl-mvp-design.md
```

### Crate 依赖图（严格分层，无环）

```
agnes-ast   ←   agnes-parser
    ↑              ↑
    └── agnes-types ← agnes-registry ← agnes-builtins
              ↑            ↑                ↑
              └── agnes-checker ← agnes-compiler ← agnes-runtime ← agnes-cli
```

每个 crate 单一职责：换 parser 只动 `agnes-parser`；换执行后端只动 `agnes-runtime`；加 tool provider 只动 `agnes-builtins`（后续可拆出 `agnes-mcp` 等）。

### 依赖（Workspace `[workspace.dependencies]` 集中管理）

- `tokio` (`rt-multi-thread`, `macros`) — 异步执行 + par 并行
- `serde` + `serde_json` — Transport Type / JSON Schema 序列化
- `lexpr`（或手写）— S-expression 解析
- `thiserror` — 错误定义
- `anyhow` — 应用层错误传播
- `tracing` — 结构化日志
- `insta`（dev）— snapshot 测试错误信息模板

---

## 六、5 个内置 Tool（用于验证）

### 内置 Trait（能力，形容词/动名词命名）

| Trait | 含义 |
|---|---|
| `Readable` | 可被读取为文本序列 |
| `Writable` | 可被写回持久化存储 |
| `Binary` | 是二进制字节流（相对于文本） |
| `OCRable` | 能被 OCR 识别为文本 |
| `Translatable` | 能被翻译（内容是自然语言） |
| `Summarizable` | 能被摘要 |

### 内置 Type（事物，名词命名）

| Type | 实现的 Trait |
|---|---|
| `Path` | (none) |
| `Text` | `Readable`, `Writable`, `Translatable`, `Summarizable` |
| `Markdown` | `Readable`, `Writable`, `Translatable`, `Summarizable` |
| `HTML` | `Readable`, `Writable`, `Translatable`, `Summarizable` |
| `PDF` | `Binary`, `OCRable` |
| `Image` | `Binary`, `OCRable` |
| `Summary` | `Readable`, `Writable` |
| `Unit` | (none) |
| `Unknown` | (none) |

### Tool 签名

| Tool | 签名 |
|---|---|
| `read-file` | `:path Path → Text` |
| `write-file` | `:path Path :content Readable → Unit` |
| `summarize` | `Summarizable → Summary` |
| `translate` | `Translatable :lang String → Text` |
| `ocr` | `OCRable + Binary → Text` |

`summarize` / `translate` / `ocr` 在 MVP 中可用 mock 实现（返回定长占位字符串），也可对接真实模型；文件读写就是 `std::fs`。

---

## 七、MVP 验证 Workflow（Acceptance Criteria）

以下 workflow 必须**编译通过、执行成功**：

**`examples/full-demo.agnes`：**
```lisp
(define read-and-translate
  :params  [(path: Path) (target: String)]
  :provides Text
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

;; 主 workflow
(pipe
  (let src (tool read-file :path "README.md"))
  (par
    (let sum (tool summarize src))
    (let ja  (tool read-and-translate :path "README.md" :target "ja")))
  (tool llm :prompt "combine summary and translation" :input sum))
```

以下必须**编译失败**并给出符合模板的错误信息：

```lisp
;; 1. 传错类型（PDF 不 Summarizable）
(pipe
  (tool read-file :path "x.pdf")     ; 假设改用产出 PDF 的 tool
  (tool summarize))                   ; requires Summarizable，PDF 只有 Binary+OCRable → 报错

;; 2. 递归 define
(define loopy :params [] :provides Unit
  (tool loopy))                       ; 编译期检测环 → 报错

;; 3. 未 declare 的 type
(pipe
  (tool ocr :source unknown-value)    ; unknown-value 是 Unknown → 报错并给 declare 修复片段
  (tool summarize))

;; 4. 命名空间冲突
(declare type Readable :implements [Readable])   ; type 名与已有 trait 冲突 → 报错
```

---

## 八、MVP 明确不做（Roadmap 保留）

- MCP / CLI / HTTP tool adapter（Phase MVP+1）
- LLM 自动推断未知 tool 的 trait（Phase MVP+2）
- 参数化 trait（`Image<png>`）、trait 组合（AND/OR）
- Optimizer 的 CSE、缓存、并行推导
- 递归 / 互递归 `define`
- Agent 壳（多轮对话、记忆、planner 循环）
- 语言服务器（LSP / 语法高亮）

# agnes DSL MVP — 设计规格

> **Status (2026-07-18 update):** the type-system portions of this spec
> (§II, §III.5, §VI's alias/param forms) have been superseded by
> `2026-07-18-agnes-type-system-upgrade-design.md`. Refer to the
> upgrade spec for current union syntax `(| A B)`, param form
> `(name Type)`, and parameterized types `(List T)` / `(Option T)`.
> This document is preserved as historical reference for the MVP
> milestone.

> 日期：2026-07-18
> 项目：agnes（Rust edition 2024，Cargo workspace）
> 类型：新语言 + Runtime 设计
> MVP 目标：**语言可行性（Language Feasibility）** —— 证明 "Lisp + Type" 是好的 agent workflow 编排抽象

---

## Context（为什么做这件事）

目标是开发一个新的 agent，使用一门专用的 workflow DSL 来编排任务，最终愿景是"达到 JavaScript 之于浏览器的效果" —— 成为 agent 编排的 canonical 语言 + runtime。

现有 agent 编排方案（LangGraph / Dify / n8n / 直接 LLM 调 tool）存在以下问题：

- 编排逻辑要么藏在代码里（不可移植），要么藏在图形拖拽里（LLM 无法自动生成）
- Tool 调用没有 **semantic type**（MCP / CLI / HTTP tool 只声明 `string / object` 这类 transport type），LLM 组合 tool 时全靠 prompt 里的自然语言描述，容易出错
- 缺少能被静态检查、能被优化、能被复用的**中间表示（Intermediate Representation）**

**agnes 的定位：** 一门 LLM 友好的、可编译为 DAG 的、带 **semantic type** 系统的 workflow DSL + runtime。LLM 只负责**规划（生成 DSL）**，runtime 负责**执行**。类型系统承担的角色类比 TypeScript 之于 JavaScript：底层 tool 无类型声明，LLM 通过 `declare` 给它们加语义类型注解，编译器基于注解检查 workflow。

**MVP 的验证目标：** 用 5 个内置 tool，写出一个通过类型检查、包含 `pipe / par / let / define / declare` 全部核心特性的 workflow，跑通端到端。证明这门语言的**抽象是对的** —— 不追求性能、生态、工具丰富度。

---

## 关键设计原则

1. **DSL 描述逻辑，Runtime 负责执行，MCP 负责连接工具，Type 负责工具组合，LLM 负责规划**
2. **LLM 是一等公民使用者** —— 语法机械化、错误信息即修复模板
3. **显式 > 隐式**，除数据流外的所有语义都显式表达
4. **DAG 保证无环**，任何递归 = 编译错误
5. **默认 fail-fast**，重试/容错必须显式
6. **类型系统 = Type + Union + Alias**（类 TypeScript）—— 没有 trait 层。理由：LLM 给 tool 加注解时，直接写 type 名（名词）远比选一组能力标签自然
7. **运行时边界校验**：编译期检查覆盖不到 tool provider 实际返回的东西，通过在跨 tool 边界处执行 type validator 补上信任缺口

---

## 一、核心语言特性（全景）

### 1.1 顶层指令（Top-level Directives）

顶层指令注册 registry 条目，**不参与执行流**。

| 指令 | 作用 |
|---|---|
| `declare type` | 声明一个 type（可选附 validator） |
| `declare type-alias` | 定义一个 type union 的命名别名 |
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
(declare type PDF)
(declare type Markdown)
(declare type-alias TextLike (PlainText | Markdown | HTML))

(declare tool ocr
  :requires [(source: (PDF | Image))]
  :provides PlainText)

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
| **Semantic Type** | agnes registry | Workflow 编译期类型推导 + 运行时边界校验 |

Transport Type **不参与** workflow 类型检查。所有 semantic 层面的判定都基于 type 名字和 union 关系。

### 2.2 类型表达式

Tool 签名和 `type-alias` 中允许出现的类型表达式：

```
TypeExpr ::= <TypeName>                    ; 单一 type
           | <AliasName>                   ; 命名别名（展开为 union）
           | (<TypeExpr> | <TypeExpr>)     ; union（可嵌套，最终扁平化）
```

Union 语义 = 该位置接受**任一**成员 type。

### 2.3 类型检查规则（就两条）

1. **参数满足（Parameter Satisfaction）**：调用点 argument 的 type 必须**属于** tool 参数声明的类型表达式（union 展开成 flat `HashSet<TypeName>` 后做 `contains`）
2. **流向满足（Flow Satisfaction）**：`pipe` 中前一步的 provides type 必须**属于** 后一步的 requires 类型表达式（仅当后一步单参数时启用隐式流）

判定极简：flatten union → `HashSet::contains(actual_type)`。

### 2.4 `declare type` / `declare type-alias`

```lisp
(declare type <Name> [:validator <spec>])   ; 定义新 type，可选运行时校验
(declare type-alias <Name> <TypeExpr>)      ; 定义命名的 union
```

- 一个名字要么是 type 要么是 alias，**同一命名空间**，重复声明报错
- 未通过 `declare` 引入的名字视为 **Unknown**（该数据点无法进入需要具体 type 的 tool）
- Runtime 缓存 declare 结果（LLM 推断的注解可持久化）

### 2.5 LLM 友好的错误信息模板

**关键设计决策：** 编译错误信息本身就是给 LLM 的修复模板。每条错误必须包含 **What / Why / Fix suggestion** 三段：

```
Type error at (tool summarize <arg>):
  parameter requires one of: PlainText, Markdown, HTML, PDF
  but got type: JSON

Fix suggestion (one of):
  A) Extract text from JSON first with a tool that provides one of the accepted types
  B) If this JSON contains prose text, extend summarize to accept it:
     (declare tool summarize :requires (PlainText | Markdown | HTML | PDF | JSON) ...)
```

命名冲突错误：

```
Name conflict: `TextLike` is already registered as a type
  attempted: (declare type-alias TextLike ...)
  suggestion: rename to `TextLikeV2` or choose a different noun
```

`declare` 的**主要使用者是 LLM 本身** —— 语法必须机械化、位置固定、可直接从错误信息生成。

### 2.6 运行时边界校验（Validated Boundaries）

编译期检查只验证"tool **声明**的 type 匹配"，无法验证"tool **实际返回**的值符合 type 契约"。跨 tool 边界处执行 validator 补上这个信任缺口。

**规则：**
- 每个 `Type` 可选附 `validator: fn(&Value) -> Result<()>`（Rust native tool 直接内嵌；MVP 阶段不做用户级 validator DSL）
- **Tool 结果返回时**：用 provides 声明的 type 的 validator 校验一次
- **Tool 参数传入前**：用 requires 声明的 type 的 validator 校验一次（当 argument 的来源不是"刚刚校验过的上游 tool 结果"时才必要——MVP 阶段简化为都校验一次）
- Validator 只校验**结构性契约**，不假装做语义判断（"是不是自然语言"等无法运行时判定的语义）
- 校验失败 = `RuntimeTypeError`，走 `retry / catch` 机制（等同于 tool error）
- Union type 的 validator = "任一成员 type 的 validator 通过即可"
- Alias 展开后按各成员校验
- 无 validator 的 type = 跳过（等价于信任模式）

**错误信息示例：**

```
Runtime type error at (tool ocr :source <arg>):
  step `read-file` declared: provides PlainText
  but returned value fails PlainText validator:
    invalid UTF-8 at byte 42 (0xC3 0x28)

This likely means:
  - Tool `read-file` implementation has a bug, OR
  - The file at "/some/path" isn't actually text (try declaring it as PDF or Image)

Fix suggestion:
  If this file is binary, redirect the workflow:
    (tool ocr :source (tool read-file-binary :path "/some/path"))
```

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
- `if` 两分支类型的 union 是整个 `if` 的类型；`match` 同理

### 3.3 错误处理（分层组合式，默认 fail-fast）

**默认行为：** 所有 tool 错误（含 `RuntimeTypeError`）立即向上传播，workflow 停止。
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
- 支持参数默认值：`(text: PlainText :default nil)`
- **MVP 不支持递归** —— 编译期做拓扑排序检测环
- 一个 `define` 注册后，与内置 tool **调用方式完全一致**

### 3.5 `declare` 的三种形态

```lisp
(declare type       <Name> [:validator <spec>])
(declare type-alias <Name> <TypeExpr>)
(declare tool       <Name>
  :requires [(<param>: <TypeExpr>) ...]
  :provides <TypeExpr>)
```

- 顶层 `declare` 覆盖或补全 Tool Registry
- 未知 tool 的初始 provides / requires 为 `Unknown`，必须通过 `declare` 补全后才能进入 workflow
- Runtime 缓存推断结果（LLM 推断的 declare 片段可持久化）

---

## 四、Runtime 架构

```
User
 │
LLM Planner              ← 生成 agnes DSL（含 declare 补全未知 tool 类型）
 │
agnes source (.agnes)
 │
agnes-parser             → S-expr AST
 │
agnes-registry           ← declare / define 注册（含命名空间冲突检查）
 │
agnes-checker            → type-in-union 判定
 │
agnes-compiler           → AST + Registry → DAG（含环检测）
 │
agnes-runtime            → 按 DAG 调度；跨 tool 边界处执行 type validator；处理 retry / catch
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
│   ├── agnes-types/                        ← Type / TypeExpr / ToolSignature / Validator
│   ├── agnes-registry/                     ← Type/Alias/Tool 注册表 + 命名冲突检查
│   ├── agnes-checker/                      ← 类型检查器 + 错误模板
│   ├── agnes-compiler/                     ← AST → DAG（define 展开、环检测）
│   ├── agnes-runtime/                      ← DAG 执行器（tokio async）+ 运行时边界校验
│   ├── agnes-builtins/                     ← 5 个内置 tool 的 Rust 实现 + 内置 type 的 validator
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

## 六、内置 Type / Tool（用于 MVP 验证）

### 内置 Type + Validator

Validator 只校验**结构性契约**（是否可解析、magic number、UTF-8 有效性），不做语义判断。

| Type | Validator |
|---|---|
| `Path` | 非空 string，不含 `\0` |
| `PlainText` | 有效 UTF-8 |
| `Markdown` | 有效 UTF-8（结构性最小，不做 Markdown 语法检查） |
| `HTML` | 有效 UTF-8 |
| `JSON` | 有效 UTF-8 且能被 `serde_json::from_str::<Value>` 解析 |
| `PDF` | 头 4 字节是 `%PDF` |
| `Image` | 头几字节是已知 magic number（PNG / JPEG / GIF / WebP） |
| `Summary` | 有效 UTF-8 |
| `Unit` | 值是 null 或空对象 |
| `Unknown` | 无 validator（跳过校验） |

### 预置 Type Alias（LLM 用得着）

```lisp
(declare type-alias TextLike (PlainText | Markdown | HTML))
(declare type-alias VisualDoc (PDF | Image))
```

### 内置 Tool 签名

| Tool | 签名 |
|---|---|
| `read-file` | `:path Path → PlainText` |
| `write-file` | `:path Path :content TextLike → Unit` |
| `summarize` | `(TextLike \| PDF) → Summary` |
| `translate` | `TextLike :lang String → PlainText` |
| `ocr` | `VisualDoc → PlainText` |
| `llm` | `PlainText :prompt String → PlainText` |

MVP 中 `summarize / translate / ocr / llm` 可用 mock 实现（返回定长占位字符串），也可对接真实模型；文件读写就是 `std::fs`。

---

## 七、MVP 验证 Workflow（Acceptance Criteria）

以下 workflow 必须**编译通过、执行成功**：

**`examples/full-demo.agnes`：**
```lisp
(define read-and-translate
  :params  [(path: Path) (target: String)]
  :provides PlainText
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

以下必须**编译失败**（或对 4：**运行时失败**）并给出符合模板的错误信息：

```lisp
;; 1. 类型不匹配
(pipe
  (tool read-file :path "x.md")     ; provides PlainText
  (tool ocr))                        ; requires VisualDoc → 报错

;; 2. 递归 define
(define loopy :params [] :provides Unit
  (tool loopy))                      ; 编译期检测环 → 报错

;; 3. 未 declare 的 type
(declare tool weird :requires MysteryType :provides PlainText)
(pipe
  (tool something-else)              ; provides SomethingElse (Unknown)
  (tool weird))                      ; requires MysteryType (Unknown) → 报错 + Fix suggestion

;; 4. 命名冲突
(declare type PlainText)             ; 已存在 → 报错 + rename suggestion

;; 5. 运行时校验失败（编译通过，运行时才报错）
(pipe
  (tool read-binary-as-text :path "x.png")   ; declared: → PlainText，实际返回 non-UTF-8
  (tool summarize))
;; → Runtime type error: PlainText validator fails at boundary of `read-binary-as-text`
```

---

## 八、MVP 明确不做（Roadmap 保留）

- MCP / CLI / HTTP tool adapter（Phase MVP+1）
- LLM 自动推断未知 tool 的类型（Phase MVP+2）
- 用户级 validator DSL（JSON Schema / regex / 自定义谓词）
- **Trait / typeclass 层**（未来若开放生态出现"数百 type 共享属性 / 参数化容器 / 方法派发"需求，可作为兼容扩展加入）
- 参数化 type（`Container<T>`）
- Optimizer 的 CSE、缓存、并行推导
- 递归 / 互递归 `define`
- Agent 壳（多轮对话、记忆、planner 循环）
- 语言服务器（LSP / 语法高亮）

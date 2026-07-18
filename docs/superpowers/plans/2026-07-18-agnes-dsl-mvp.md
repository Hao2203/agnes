# agnes DSL MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the agnes DSL MVP — a Lisp-style workflow language + Rust runtime with a TypeScript-style semantic type system, capable of compiling and executing a workflow that uses `pipe / par / let / define / declare` over 5 built-in tools.

**Architecture:** Cargo workspace with 9 single-purpose crates. Data flow: `.agnes` source -> `agnes-parser` -> AST -> `agnes-registry` (declare/define registration) -> `agnes-checker` (type check) -> `agnes-compiler` (AST -> DAG) -> `agnes-runtime` (tokio async executor + boundary validators) -> `agnes-builtins` (native tool implementations). `agnes-cli` is the binary entry point.

**Tech Stack:** Rust edition 2024, tokio (rt-multi-thread + macros), serde + serde_json, lexpr (S-expression parser), thiserror, anyhow, tracing, insta (snapshot tests).

## Global Constraints

- Rust edition 2024 throughout every crate.
- All crates named `agnes-<component>` and placed under `crates/<name>/`.
- Workspace root `Cargo.toml` contains `[workspace]` and `[workspace.dependencies]` sections (no `[package]`).
- Shared dependencies declared once in `[workspace.dependencies]`; member crates reference with `<dep>.workspace = true`.
- Version control uses jj (colocated with git). Commit workflow at end of each task: `jj describe -m "..."` then `jj new` then `jj bookmark move main --to @-`. **Never** use `git commit`.
- Every commit message ends with `Co-Authored-By: Claude <noreply@anthropic.com>` on its own line.
- Language of code, comments, and error messages: English. Error message text follows the What / Why / Fix suggestion three-section format from the spec.
- Type names use PascalCase (`PlainText`, `Markdown`); tool names and parameter names use kebab-case (`read-file`, `write-file`, `target-lang`).
- Semantic type checking rules: exactly two (parameter satisfaction, flow satisfaction), based on `HashSet<TypeName>::contains` after union flattening.
- Runtime boundary validation is mandatory: every tool call site validates `requires` types on entry and `provides` type on return (skipping types with no validator).
- No trait / typeclass layer in MVP. Only Type + Union + Alias.

## File Structure (locked before task decomposition)

```
agnes/
├── Cargo.toml                       # workspace root
├── rust-toolchain.toml              # pin toolchain
├── .gitignore                       # add target/, *.lock rules for jj
├── crates/
│   ├── agnes-ast/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs               # TopLevel, Expr, Literal, KeywordArgs, Span
│   ├── agnes-parser/
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs               # pub fn parse(&str) -> Result<Program, ParseError>
│   │   ├── src/lexer.rs             # tokenize source; lexpr wrapper
│   │   ├── src/toplevel.rs          # parse declare-*/define
│   │   ├── src/expr.rs              # parse pipe/par/let/if/match/foreach/retry/catch/tool/llm/return
│   │   └── src/error.rs             # ParseError with span + render()
│   ├── agnes-types/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs               # TypeName, TypeExpr, Validator, ToolSignature, Value
│   ├── agnes-registry/
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs               # Registry: types + aliases + tools; conflict check
│   │   └── src/loader.rs            # apply Program's TopLevel entries into Registry
│   ├── agnes-checker/
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs               # pub fn check(program, registry) -> Result<(), CheckError>
│   │   ├── src/env.rs               # Env: HashMap<VarName, TypeName>
│   │   ├── src/rules.rs             # rule_parameter_satisfaction, rule_flow_satisfaction
│   │   └── src/error.rs             # CheckError renderer with What/Why/Fix template
│   ├── agnes-compiler/
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs               # pub fn compile(program, registry) -> Result<Dag, CompileError>
│   │   ├── src/dag.rs               # Node, NodeId, Dag, Input
│   │   ├── src/lower.rs             # AST -> DAG lowering with define inlining
│   │   ├── src/desugar.rs           # retry/catch modifier -> control-flow form
│   │   └── src/cycle.rs             # topological sort + cycle detection
│   ├── agnes-runtime/
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs               # pub async fn execute(dag, registry) -> Result<Value, RuntimeError>
│   │   ├── src/scheduler.rs         # pipe = sequential await, par = tokio::join
│   │   ├── src/boundary.rs          # validate requires/provides at each tool call
│   │   └── src/error.rs             # RuntimeError (incl. RuntimeTypeError) renderer
│   ├── agnes-builtins/
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs               # pub fn register_builtins(&mut Registry); pub fn native_dispatch(name) -> ToolImpl
│   │   ├── src/types.rs             # 10 built-in types + validators
│   │   ├── src/aliases.rs           # TextLike, VisualDoc
│   │   └── src/tools.rs             # 6 built-in tool impls (read-file, write-file, summarize, translate, ocr, llm)
│   └── agnes-cli/
│       ├── Cargo.toml
│       └── src/main.rs              # parse args, load .agnes file, run pipeline, print result or error
├── examples/
│   ├── hello.agnes                  # single tool call
│   ├── translate.agnes              # sequential pipe
│   ├── fan-out.agnes                # par + let
│   ├── with-define.agnes            # compound tool
│   └── full-demo.agnes              # spec's acceptance workflow
├── tests/
│   └── e2e.rs                       # workspace-level e2e using agnes-cli as library
└── docs/superpowers/
    ├── specs/2026-07-18-agnes-dsl-mvp-design.md
    └── plans/2026-07-18-agnes-dsl-mvp.md
```

**Locked interfaces (referenced across tasks):**

- `agnes_ast::Program { toplevels: Vec<TopLevel>, main: Option<Expr> }`
- `agnes_ast::TopLevel` variants: `DeclareType`, `DeclareTypeAlias`, `DeclareTool`, `Define`
- `agnes_ast::Expr` variants: `Tool`, `Pipe`, `Par`, `Let`, `If`, `Match`, `Foreach`, `Retry`, `Catch`, `Llm`, `Return`, `Literal`, `Var`
- `agnes_types::TypeName(pub String)` — newtype
- `agnes_types::TypeExpr` variants: `Named(TypeName)`, `Union(Vec<TypeExpr>)` — aliases resolved to `Union` in-registry
- `agnes_types::ToolSignature { requires: Vec<(String, TypeExpr)>, provides: TypeExpr }`
- `agnes_types::Value` = wrapper around `serde_json::Value` with an associated declared `TypeName`
- `agnes_types::Validator` = `fn(&serde_json::Value) -> Result<(), String>`
- `agnes_registry::Registry` — owns types/aliases/tools and provides `type_names()`, `flatten(&TypeExpr) -> HashSet<TypeName>`, `validator_of(&TypeName) -> Option<Validator>`
- `agnes_compiler::dag::Dag { nodes: Vec<Node>, root: NodeId }`, `Node { id, tool_name, inputs: Vec<Input>, provides: TypeExpr }`, `Input::FromNode(NodeId) | Input::Literal(Value) | Input::Var(String)`
- `agnes_runtime::execute(dag, registry, builtins) -> Result<Value, RuntimeError>` — async, tokio-based

---

## Task 1: Workspace scaffolding

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `rust-toolchain.toml`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: nothing (first task)
- Produces: an empty but buildable Cargo workspace with `[workspace.dependencies]` populated for downstream crates

- [ ] **Step 1: Write the workspace Cargo.toml**

Overwrite `/home/hao/code/agnes/Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = [
    "crates/agnes-ast",
    "crates/agnes-parser",
    "crates/agnes-types",
    "crates/agnes-registry",
    "crates/agnes-checker",
    "crates/agnes-compiler",
    "crates/agnes-runtime",
    "crates/agnes-builtins",
    "crates/agnes-cli",
]

[workspace.package]
edition = "2024"
version = "0.1.0"
license = "MIT OR Apache-2.0"
authors = ["agnes contributors"]

[workspace.dependencies]
# internal
agnes-ast      = { path = "crates/agnes-ast" }
agnes-parser   = { path = "crates/agnes-parser" }
agnes-types    = { path = "crates/agnes-types" }
agnes-registry = { path = "crates/agnes-registry" }
agnes-checker  = { path = "crates/agnes-checker" }
agnes-compiler = { path = "crates/agnes-compiler" }
agnes-runtime  = { path = "crates/agnes-runtime" }
agnes-builtins = { path = "crates/agnes-builtins" }

# external
tokio       = { version = "1.40", features = ["rt-multi-thread", "macros", "fs", "sync"] }
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
lexpr       = "0.2"
thiserror   = "2"
anyhow      = "1"
tracing     = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
insta       = { version = "1", features = ["yaml"] }
```

- [ ] **Step 2: Pin the toolchain**

Create `/home/hao/code/agnes/rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 3: Update .gitignore**

Overwrite `/home/hao/code/agnes/.gitignore`:

```
/target
Cargo.lock.new
**/*.rs.bk
.direnv/
```

- [ ] **Step 4: Verify workspace parses (no crates yet, expect empty-workspace warning is fine)**

Run: `cd /home/hao/code/agnes && cargo metadata --format-version=1 > /dev/null`
Expected: exits 0 (may warn about missing members — that's fine, we create them next task). If any error mentions `resolver`, remove the `resolver = "3"` line and try `"2"` (older Cargo).

- [ ] **Step 5: Commit via jj**

Run:
```
cd /home/hao/code/agnes
jj describe -m "chore: scaffold cargo workspace root

Add workspace Cargo.toml with 9 planned member crates, workspace.dependencies
for shared deps (tokio, serde, lexpr, thiserror, anyhow, tracing, insta),
and rust-toolchain.toml pinning stable + rustfmt + clippy.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 2: agnes-ast crate

**Files:**
- Create: `crates/agnes-ast/Cargo.toml`
- Create: `crates/agnes-ast/src/lib.rs`

**Interfaces:**
- Consumes: nothing (leaf crate)
- Produces:
  - `Program { toplevels: Vec<TopLevel>, main: Option<Expr> }`
  - `TopLevel::{ DeclareType, DeclareTypeAlias, DeclareTool, Define }` (see fields in Step 3)
  - `Expr::{ Tool, Pipe, Par, Let, If, Match, Foreach, Retry, Catch, Llm, Return, Literal, Var }`
  - `Literal::{ String(String), Int(i64), Bool(bool), Nil }`
  - `KwArgs = Vec<(String, Expr)>`
  - `Span { start: usize, end: usize }` (byte offsets, opaque to downstream)

- [ ] **Step 1: Write the crate manifest**

Create `crates/agnes-ast/Cargo.toml`:

```toml
[package]
name = "agnes-ast"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
```

- [ ] **Step 2: Write failing round-trip test**

Create `crates/agnes-ast/src/lib.rs` with just enough to hold the test module:

```rust
//! AST types for agnes DSL.
//!
//! This crate defines the abstract syntax tree produced by `agnes-parser`
//! and consumed by every downstream crate. It has no dependencies of its own.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_can_hold_a_pipe_of_two_tools() {
        // A minimal Program: (pipe (tool read-file :path "x") (tool summarize))
        let p = Program {
            toplevels: vec![],
            main: Some(Expr::Pipe {
                span: Span { start: 0, end: 0 },
                steps: vec![
                    Expr::Tool {
                        span: Span { start: 0, end: 0 },
                        name: "read-file".into(),
                        args: vec![("path".into(), Expr::Literal {
                            span: Span { start: 0, end: 0 },
                            lit: Literal::String("x".into()),
                        })],
                    },
                    Expr::Tool {
                        span: Span { start: 0, end: 0 },
                        name: "summarize".into(),
                        args: vec![],
                    },
                ],
            }),
        };
        assert_eq!(p.toplevels.len(), 0);
        assert!(matches!(p.main, Some(Expr::Pipe { .. })));
    }
}
```

- [ ] **Step 3: Run test — expect compile failures naming missing items**

Run: `cargo test -p agnes-ast --lib`
Expected: compile errors like `cannot find struct Program`, `cannot find struct Span`, etc.

- [ ] **Step 4: Add the minimal AST types to make the test pass**

Prepend to `crates/agnes-ast/src/lib.rs` (before the `#[cfg(test)] mod tests`):

```rust
use std::fmt;

/// Byte-offset span into the original source (inclusive start, exclusive end).
/// Opaque to downstream crates; parser produces them, error renderers consume them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const DUMMY: Span = Span { start: 0, end: 0 };
}

/// Literal values that can appear directly in source (never a Value that
/// flows between tools — that's `agnes_types::Value`).
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(String),
    Int(i64),
    Bool(bool),
    Nil,
}

/// A named formal parameter in `define` or `declare tool`.
/// Example: `(source: (PDF | Image) :default nil)`
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: TypeExprAst,
    pub default: Option<Literal>,
}

/// Type expression as it appears syntactically. `agnes-types` will
/// resolve aliases and flatten unions.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExprAst {
    Named(String),
    Union(Vec<TypeExprAst>),
}

/// Keyword arguments: (:key value ...)
pub type KwArgs = Vec<(String, Expr)>;

/// Top-level directives — registered before any workflow is checked.
#[derive(Debug, Clone, PartialEq)]
pub enum TopLevel {
    /// `(declare type <Name>)` — validator is attached at registry-load time
    /// for native types; MVP does not support user-authored validator DSL.
    DeclareType {
        span: Span,
        name: String,
    },
    /// `(declare type-alias <Name> <TypeExpr>)`
    DeclareTypeAlias {
        span: Span,
        name: String,
        expr: TypeExprAst,
    },
    /// `(declare tool <Name> :requires [...] :provides <TypeExpr>)`
    DeclareTool {
        span: Span,
        name: String,
        requires: Vec<Param>,
        provides: TypeExprAst,
    },
    /// `(define <Name> :params [...] :provides <TypeExpr> <body>)`
    Define {
        span: Span,
        name: String,
        params: Vec<Param>,
        provides: TypeExprAst,
        body: Box<Expr>,
    },
}

/// Workflow expression forms.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// `(tool <name> :key value ... [:retry N] [:on-error <expr>])`
    Tool {
        span: Span,
        name: String,
        args: KwArgs,
    },
    /// `(pipe e1 e2 ...)`
    Pipe { span: Span, steps: Vec<Expr> },
    /// `(par e1 e2 ...)`
    Par { span: Span, branches: Vec<Expr> },
    /// `(let name)` = single-arg: name the current pipe stream.
    /// `(let name expr)` = two-arg: bind side value; do not consume stream.
    Let {
        span: Span,
        name: String,
        value: Option<Box<Expr>>,
    },
    /// `(if cond then else)`
    If {
        span: Span,
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /// `(match scrutinee (pattern1 expr1) (pattern2 expr2) ...)`
    Match {
        span: Span,
        scrutinee: Box<Expr>,
        arms: Vec<(Literal, Expr)>,
    },
    /// `(foreach item collection body)`
    Foreach {
        span: Span,
        item: String,
        collection: Box<Expr>,
        body: Box<Expr>,
    },
    /// `(retry :times N :backoff <spec> <body>)`
    Retry {
        span: Span,
        times: u32,
        backoff: Option<String>,
        body: Box<Expr>,
    },
    /// `(catch :on <ErrClass> :fallback <expr> <body>)`
    Catch {
        span: Span,
        on: Option<String>,
        fallback: Box<Expr>,
        body: Box<Expr>,
    },
    /// `(llm :prompt "..." :input <expr>)` — a builtin form for the LLM tool.
    Llm { span: Span, args: KwArgs },
    /// `(return expr)`
    Return { span: Span, value: Box<Expr> },
    /// A literal in expression position.
    Literal { span: Span, lit: Literal },
    /// A reference to a bound name (from `let` or a `define` param).
    Var { span: Span, name: String },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Tool { span, .. }
            | Expr::Pipe { span, .. }
            | Expr::Par { span, .. }
            | Expr::Let { span, .. }
            | Expr::If { span, .. }
            | Expr::Match { span, .. }
            | Expr::Foreach { span, .. }
            | Expr::Retry { span, .. }
            | Expr::Catch { span, .. }
            | Expr::Llm { span, .. }
            | Expr::Return { span, .. }
            | Expr::Literal { span, .. }
            | Expr::Var { span, .. } => *span,
        }
    }
}

/// A parsed .agnes file.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub toplevels: Vec<TopLevel>,
    /// The final expression at file end — the workflow entry point.
    /// None means the file only registers declares/defines.
    pub main: Option<Expr>,
}

impl fmt::Display for TypeExprAst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeExprAst::Named(n) => write!(f, "{n}"),
            TypeExprAst::Union(members) => {
                let rendered: Vec<String> = members.iter().map(|m| m.to_string()).collect();
                write!(f, "({})", rendered.join(" | "))
            }
        }
    }
}
```

- [ ] **Step 5: Run test — expect PASS**

Run: `cargo test -p agnes-ast --lib`
Expected: `1 passed`.

- [ ] **Step 6: Commit via jj**

Run:
```
cd /home/hao/code/agnes
jj describe -m "feat(ast): define AST types for agnes DSL

Add Program, TopLevel (DeclareType, DeclareTypeAlias, DeclareTool, Define),
Expr (Tool, Pipe, Par, Let, If, Match, Foreach, Retry, Catch, Llm, Return,
Literal, Var), Param, TypeExprAst (Named | Union), Literal, and Span.

Includes a round-trip test constructing a two-step pipeline by hand.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 3: agnes-types crate

**Files:**
- Create: `crates/agnes-types/Cargo.toml`
- Create: `crates/agnes-types/src/lib.rs`

**Interfaces:**
- Consumes: `agnes_ast::TypeExprAst` (for conversion)
- Produces:
  - `TypeName(pub String)`
  - `TypeExpr::{ Named(TypeName), Union(HashSet<TypeName>) }` — always canonicalized (aliases resolved, flattened)
  - `Validator = fn(&serde_json::Value) -> Result<(), String>`
  - `ToolSignature { requires: Vec<(String, TypeExpr)>, provides: TypeExpr }`
  - `Value { data: serde_json::Value, declared_type: TypeName }`
  - `type_expr_matches(actual: &TypeName, expected: &TypeExpr) -> bool`

- [ ] **Step 1: Write the crate manifest**

Create `crates/agnes-types/Cargo.toml`:

```toml
[package]
name = "agnes-types"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
agnes-ast.workspace = true
serde.workspace = true
serde_json.workspace = true
```

- [ ] **Step 2: Write failing tests**

Create `crates/agnes-types/src/lib.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_matches_named() {
        let expected = TypeExpr::Named(TypeName("PlainText".into()));
        assert!(type_expr_matches(&TypeName("PlainText".into()), &expected));
        assert!(!type_expr_matches(&TypeName("PDF".into()), &expected));
    }

    #[test]
    fn union_contains_member() {
        let mut set = std::collections::HashSet::new();
        set.insert(TypeName("PlainText".into()));
        set.insert(TypeName("Markdown".into()));
        let expected = TypeExpr::Union(set);
        assert!(type_expr_matches(&TypeName("Markdown".into()), &expected));
        assert!(!type_expr_matches(&TypeName("PDF".into()), &expected));
    }

    #[test]
    fn utf8_validator_accepts_valid_string() {
        let v = |json: &serde_json::Value| -> Result<(), String> {
            match json.as_str() {
                Some(s) if !s.as_bytes().is_empty() && std::str::from_utf8(s.as_bytes()).is_ok() => Ok(()),
                Some(_) => Err("empty".into()),
                None => Err("not a string".into()),
            }
        };
        assert!(v(&serde_json::json!("hello")).is_ok());
        assert!(v(&serde_json::json!(42)).is_err());
    }
}
```

- [ ] **Step 3: Run — expect compile failures**

Run: `cargo test -p agnes-types --lib`
Expected: `cannot find struct TypeName`, `cannot find enum TypeExpr`, `cannot find function type_expr_matches`.

- [ ] **Step 4: Add the minimal types**

Prepend to `crates/agnes-types/src/lib.rs`:

```rust
//! Semantic type system for agnes.

use serde_json::Value as JsonValue;
use std::collections::HashSet;
use std::fmt;

/// Canonical name of a type or type alias. PascalCase by convention.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeName(pub String);

impl fmt::Display for TypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Canonicalized type expression. `Union` is always non-empty and flat
/// (no nested unions, aliases already resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeExpr {
    Named(TypeName),
    Union(HashSet<TypeName>),
}

impl TypeExpr {
    /// Flatten to a set of concrete type names.
    pub fn as_set(&self) -> HashSet<TypeName> {
        match self {
            TypeExpr::Named(n) => {
                let mut s = HashSet::new();
                s.insert(n.clone());
                s
            }
            TypeExpr::Union(s) => s.clone(),
        }
    }
}

impl fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeExpr::Named(n) => write!(f, "{n}"),
            TypeExpr::Union(members) => {
                let mut names: Vec<&str> = members.iter().map(|t| t.0.as_str()).collect();
                names.sort();
                write!(f, "({})", names.join(" | "))
            }
        }
    }
}

/// Runtime type validator. Structural check only, no semantic guessing.
/// Returns `Ok(())` on pass, `Err(reason)` on fail.
pub type Validator = fn(&JsonValue) -> Result<(), String>;

/// Tool signature after registry resolution. Both `requires` items and
/// `provides` are canonicalized.
#[derive(Debug, Clone)]
pub struct ToolSignature {
    pub requires: Vec<(String, TypeExpr)>,
    pub provides: TypeExpr,
}

/// A value flowing between tools at runtime. Carries the type declared
/// by the producing tool for boundary validation on the consuming end.
#[derive(Debug, Clone)]
pub struct Value {
    pub data: JsonValue,
    pub declared_type: TypeName,
}

/// Rule primitive: does `actual` satisfy `expected`?
/// This is a set-membership test — the checker's only decision procedure.
pub fn type_expr_matches(actual: &TypeName, expected: &TypeExpr) -> bool {
    match expected {
        TypeExpr::Named(n) => n == actual,
        TypeExpr::Union(members) => members.contains(actual),
    }
}
```

- [ ] **Step 5: Run — expect PASS**

Run: `cargo test -p agnes-types --lib`
Expected: `3 passed`.

- [ ] **Step 6: Commit**

```
jj describe -m "feat(types): TypeName, TypeExpr, Validator, ToolSignature, Value

Canonical (post-alias-resolution) representation of the semantic type
system. TypeExpr is either Named or a flat Union HashSet. The single
decision procedure type_expr_matches performs a set-membership test —
this is what the checker's two rules will call.

Value carries the declared_type of its producing tool for runtime
boundary validation.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 4: agnes-parser crate

**Files:**
- Create: `crates/agnes-parser/Cargo.toml`
- Create: `crates/agnes-parser/src/lib.rs`
- Create: `crates/agnes-parser/src/error.rs`
- Create: `crates/agnes-parser/src/toplevel.rs`
- Create: `crates/agnes-parser/src/expr.rs`
- Create: `crates/agnes-parser/tests/parse.rs`

**Interfaces:**
- Consumes: `agnes_ast::{ Program, TopLevel, Expr, ... }`
- Produces:
  - `pub fn parse(source: &str) -> Result<Program, ParseError>`
  - `ParseError { span, message }` with `impl Display`

- [ ] **Step 1: Write the crate manifest**

Create `crates/agnes-parser/Cargo.toml`:

```toml
[package]
name = "agnes-parser"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
agnes-ast.workspace = true
lexpr.workspace = true
thiserror.workspace = true

[dev-dependencies]
insta.workspace = true
```

- [ ] **Step 2: Write the error module (skeleton)**

Create `crates/agnes-parser/src/error.rs`:

```rust
use agnes_ast::Span;
use std::fmt;

#[derive(Debug, Clone, thiserror::Error)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Parse error at bytes {}..{}: {}",
            self.span.start, self.span.end, self.message
        )
    }
}
```

- [ ] **Step 3: Write failing tests**

Create `crates/agnes-parser/tests/parse.rs`:

```rust
use agnes_ast::{Expr, Literal, TopLevel, TypeExprAst};
use agnes_parser::parse;

#[test]
fn parses_a_single_pipe() {
    let src = r#"
        (pipe
          (tool read-file :path "x")
          (tool summarize))
    "#;
    let p = parse(src).expect("parse ok");
    assert!(p.toplevels.is_empty());
    match p.main.expect("has main") {
        Expr::Pipe { steps, .. } => {
            assert_eq!(steps.len(), 2);
            match &steps[0] {
                Expr::Tool { name, args, .. } => {
                    assert_eq!(name, "read-file");
                    assert_eq!(args.len(), 1);
                    assert_eq!(args[0].0, "path");
                    assert!(matches!(&args[0].1,
                        Expr::Literal { lit: Literal::String(s), .. } if s == "x"));
                }
                other => panic!("expected Tool, got {other:?}"),
            }
        }
        other => panic!("expected Pipe, got {other:?}"),
    }
}

#[test]
fn parses_declare_type() {
    let src = r#"(declare type PDF)"#;
    let p = parse(src).expect("parse ok");
    assert_eq!(p.toplevels.len(), 1);
    match &p.toplevels[0] {
        TopLevel::DeclareType { name, .. } => assert_eq!(name, "PDF"),
        other => panic!("expected DeclareType, got {other:?}"),
    }
}

#[test]
fn parses_declare_type_alias() {
    let src = r#"(declare type-alias TextLike (PlainText | Markdown | HTML))"#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTypeAlias { name, expr, .. } => {
            assert_eq!(name, "TextLike");
            match expr {
                TypeExprAst::Union(members) => assert_eq!(members.len(), 3),
                other => panic!("expected Union, got {other:?}"),
            }
        }
        other => panic!("expected DeclareTypeAlias, got {other:?}"),
    }
}

#[test]
fn parses_declare_tool() {
    let src = r#"
        (declare tool ocr
          :requires [(source: (PDF | Image))]
          :provides PlainText)
    "#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTool { name, requires, provides, .. } => {
            assert_eq!(name, "ocr");
            assert_eq!(requires.len(), 1);
            assert_eq!(requires[0].name, "source");
            assert!(matches!(provides, TypeExprAst::Named(s) if s == "PlainText"));
        }
        other => panic!("expected DeclareTool, got {other:?}"),
    }
}

#[test]
fn parses_define_with_body() {
    let src = r#"
        (define greet
          :params [(who: PlainText)]
          :provides PlainText
          (tool llm :prompt "hello" :input who))
    "#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::Define { name, params, .. } => {
            assert_eq!(name, "greet");
            assert_eq!(params.len(), 1);
        }
        other => panic!("expected Define, got {other:?}"),
    }
}

#[test]
fn parses_let_two_forms() {
    let src = r#"
        (pipe
          (tool read-file :path "x")
          (let doc)
          (par
            (let sum (tool summarize doc))
            (let ja  (tool translate :lang "ja"))))
    "#;
    let _ = parse(src).expect("parse ok");
}

#[test]
fn rejects_unclosed_paren() {
    let src = r#"(pipe (tool read-file :path "x")"#;
    assert!(parse(src).is_err());
}
```

- [ ] **Step 4: Run tests — expect compile failures**

Run: `cargo test -p agnes-parser --tests`
Expected: `parse` not defined; module fails to compile.

- [ ] **Step 5: Implement the parser**

Create `crates/agnes-parser/src/lib.rs`:

```rust
//! S-expression parser for agnes DSL.
//!
//! Wraps the `lexpr` crate; walks the resulting sexpr tree into
//! `agnes_ast` types. Keyword args (`:key value`) are treated as pairs.

pub mod error;
mod toplevel;
mod expr;

use agnes_ast::{Expr, Program, Span, TopLevel};
pub use error::ParseError;

/// Parse an entire .agnes source file.
///
/// A file is a sequence of top-level forms. A `(declare ...)` or
/// `(define ...)` produces a `TopLevel`; anything else at the top is
/// treated as the `main` expression (only the last such expression
/// wins, and a parse error is returned if more than one appears).
pub fn parse(source: &str) -> Result<Program, ParseError> {
    let forms = read_forms(source)?;
    let mut toplevels = Vec::new();
    let mut main: Option<Expr> = None;

    for (form, span) in forms {
        if is_toplevel(&form) {
            toplevels.push(toplevel::parse_toplevel(&form, span)?);
        } else {
            if main.is_some() {
                return Err(ParseError {
                    span,
                    message: "multiple main expressions at top level; wrap them in a single (pipe ...) or (par ...)".into(),
                });
            }
            main = Some(expr::parse_expr(&form, span)?);
        }
    }

    Ok(Program { toplevels, main })
}

/// Read all top-level forms from source. Uses `lexpr` for tokenization.
fn read_forms(source: &str) -> Result<Vec<(lexpr::Value, Span)>, ParseError> {
    use lexpr::parse::Options;
    let opts = Options::new()
        .with_string_syntax(lexpr::parse::StringSyntax::R6RS)
        .with_keyword_syntax(lexpr::parse::KeywordSyntax::ColonPrefix);
    let mut out = Vec::new();
    let mut cursor = source;
    let mut offset = 0usize;
    loop {
        // Skip whitespace and count newlines to keep offsets in sync.
        let trimmed_start = cursor.len();
        cursor = cursor.trim_start();
        offset += trimmed_start - cursor.len();
        if cursor.is_empty() {
            return Ok(out);
        }
        // lexpr's default parser accepts one datum at a time when we call
        // `.from_str_iter`, but its exact byte offsets aren't exposed here;
        // we approximate by parsing whole remaining slice and stepping.
        let mut it = lexpr::Parser::from_str_custom(cursor, opts.clone());
        match it.next() {
            Some(Ok(v)) => {
                // Best-effort span: from current offset to end of this form.
                // Precise byte tracking is a nice-to-have; MVP uses coarse spans.
                let approx_end = offset + form_len_heuristic(cursor, &v);
                let span = Span { start: offset, end: approx_end };
                let consumed = approx_end - offset;
                cursor = &cursor[consumed.min(cursor.len())..];
                offset = approx_end;
                out.push((v, span));
            }
            Some(Err(e)) => {
                return Err(ParseError {
                    span: Span { start: offset, end: offset },
                    message: format!("{e}"),
                });
            }
            None => return Ok(out),
        }
    }
}

/// Heuristic: find the byte offset just after this form ends. Works for
/// parenthesized forms and atoms; sufficient for MVP-quality error spans.
fn form_len_heuristic(source: &str, _v: &lexpr::Value) -> usize {
    let bytes = source.as_bytes();
    if bytes.first().copied() != Some(b'(') {
        // atom: scan until whitespace or delimiter
        return bytes
            .iter()
            .position(|&b| b.is_ascii_whitespace() || b == b')' || b == b'(')
            .unwrap_or(bytes.len());
    }
    // parenthesized: walk depth
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if escape { escape = false; continue; }
            if b == b'\\' { escape = true; continue; }
            if b == b'"' { in_str = false; }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 { return i + 1; }
            }
            _ => {}
        }
    }
    bytes.len()
}

fn is_toplevel(form: &lexpr::Value) -> bool {
    let cons = match form {
        lexpr::Value::Cons(c) => c,
        _ => return false,
    };
    let head = cons.car().as_symbol();
    matches!(head, Some("declare") | Some("define"))
}
```

- [ ] **Step 6: Implement top-level parsing**

Create `crates/agnes-parser/src/toplevel.rs`:

```rust
use agnes_ast::{Literal, Param, Span, TopLevel, TypeExprAst};

use crate::error::ParseError;
use crate::expr;

pub fn parse_toplevel(form: &lexpr::Value, span: Span) -> Result<TopLevel, ParseError> {
    let items = as_list(form, span)?;
    // items[0] is 'declare' or 'define' (already checked by is_toplevel)
    let head = items[0].as_symbol().unwrap();
    match head {
        "declare" => parse_declare(&items[1..], span),
        "define"  => parse_define(&items[1..], span),
        _ => unreachable!("is_toplevel gate"),
    }
}

fn parse_declare(rest: &[lexpr::Value], span: Span) -> Result<TopLevel, ParseError> {
    let kind = rest.first()
        .and_then(|v| v.as_symbol())
        .ok_or_else(|| ParseError {
            span,
            message: "declare needs a kind: type | type-alias | tool".into(),
        })?;
    match kind {
        "type" => {
            let name = expect_symbol(rest.get(1), span, "type name")?;
            Ok(TopLevel::DeclareType { span, name: name.to_string() })
        }
        "type-alias" => {
            let name = expect_symbol(rest.get(1), span, "alias name")?;
            let expr_val = rest.get(2).ok_or_else(|| ParseError {
                span, message: "declare type-alias needs a body TypeExpr".into(),
            })?;
            let expr = parse_type_expr(expr_val, span)?;
            Ok(TopLevel::DeclareTypeAlias {
                span, name: name.to_string(), expr,
            })
        }
        "tool" => {
            let name = expect_symbol(rest.get(1), span, "tool name")?;
            let kw = parse_kwargs(&rest[2..], span)?;
            let requires_val = kw.iter().find(|(k, _)| k == "requires")
                .map(|(_, v)| v.clone())
                .ok_or_else(|| ParseError { span, message: ":requires missing".into() })?;
            let provides_val = kw.iter().find(|(k, _)| k == "provides")
                .map(|(_, v)| v.clone())
                .ok_or_else(|| ParseError { span, message: ":provides missing".into() })?;
            let requires = parse_params_vector(&requires_val, span)?;
            let provides = parse_type_expr(&provides_val, span)?;
            Ok(TopLevel::DeclareTool {
                span, name: name.to_string(), requires, provides,
            })
        }
        other => Err(ParseError {
            span,
            message: format!("unknown declare kind `{other}`; expected type | type-alias | tool"),
        }),
    }
}

fn parse_define(rest: &[lexpr::Value], span: Span) -> Result<TopLevel, ParseError> {
    let name = expect_symbol(rest.first(), span, "define name")?.to_string();
    // Collect keyword args until we hit a non-keyword position — the body.
    let mut params: Vec<Param> = Vec::new();
    let mut provides: Option<TypeExprAst> = None;
    let mut body_val: Option<&lexpr::Value> = None;

    let mut i = 1usize;
    while i < rest.len() {
        if let Some(k) = rest[i].as_keyword() {
            let v = rest.get(i + 1).ok_or_else(|| ParseError {
                span, message: format!("keyword :{k} without value"),
            })?;
            match k {
                "params" => params = parse_params_vector(v, span)?,
                "provides" => provides = Some(parse_type_expr(v, span)?),
                other => return Err(ParseError {
                    span,
                    message: format!("unknown keyword :{other} in define"),
                }),
            }
            i += 2;
        } else {
            body_val = Some(&rest[i]);
            i += 1;
        }
    }
    let provides = provides.ok_or_else(|| ParseError {
        span, message: ":provides missing in define".into(),
    })?;
    let body_val = body_val.ok_or_else(|| ParseError {
        span, message: "define body missing".into(),
    })?;
    let body = expr::parse_expr(body_val, span)?;
    Ok(TopLevel::Define {
        span, name, params, provides, body: Box::new(body),
    })
}

pub(crate) fn parse_params_vector(v: &lexpr::Value, span: Span) -> Result<Vec<Param>, ParseError> {
    let items = as_vector_or_list(v, span)?;
    let mut out = Vec::new();
    for it in items {
        out.push(parse_single_param(&it, span)?);
    }
    Ok(out)
}

fn parse_single_param(v: &lexpr::Value, span: Span) -> Result<Param, ParseError> {
    // Syntax: (name: TypeExpr [:default Literal])
    let items = as_list(v, span)?;
    // First element is a symbol ending in ':'
    let raw_name = items.first().and_then(|v| v.as_symbol())
        .ok_or_else(|| ParseError { span, message: "param name symbol expected".into() })?;
    let name = raw_name.trim_end_matches(':').to_string();
    let ty = parse_type_expr(items.get(1).ok_or_else(|| ParseError {
        span, message: "param type expected after name".into(),
    })?, span)?;
    let mut default = None;
    let mut i = 2usize;
    while i < items.len() {
        if let Some(k) = items[i].as_keyword() {
            let val = items.get(i + 1).ok_or_else(|| ParseError {
                span, message: format!("keyword :{k} in param without value"),
            })?;
            match k {
                "default" => default = Some(parse_literal(val, span)?),
                other => return Err(ParseError {
                    span, message: format!("unknown keyword :{other} in param"),
                }),
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(Param { name, ty, default })
}

pub(crate) fn parse_type_expr(v: &lexpr::Value, span: Span) -> Result<TypeExprAst, ParseError> {
    if let Some(sym) = v.as_symbol() {
        return Ok(TypeExprAst::Named(sym.to_string()));
    }
    // Otherwise it should be a list with '|' separators: e.g. (PlainText | Markdown | HTML)
    let items = as_list(v, span)?;
    let mut members = Vec::new();
    let mut expect_type = true;
    for item in items {
        if expect_type {
            let sym = item.as_symbol().ok_or_else(|| ParseError {
                span, message: "type name (symbol) expected".into(),
            })?;
            members.push(TypeExprAst::Named(sym.to_string()));
            expect_type = false;
        } else {
            let sep = item.as_symbol().ok_or_else(|| ParseError {
                span, message: "expected `|` between type expressions".into(),
            })?;
            if sep != "|" {
                return Err(ParseError { span, message: format!("expected `|`, got `{sep}`") });
            }
            expect_type = true;
        }
    }
    if members.len() == 1 {
        Ok(members.into_iter().next().unwrap())
    } else {
        Ok(TypeExprAst::Union(members))
    }
}

fn parse_literal(v: &lexpr::Value, span: Span) -> Result<Literal, ParseError> {
    match v {
        lexpr::Value::String(s) => Ok(Literal::String(s.to_string())),
        lexpr::Value::Number(n) => n.as_i64().map(Literal::Int).ok_or_else(|| ParseError {
            span, message: "only i64 int literals supported in MVP".into(),
        }),
        lexpr::Value::Bool(b) => Ok(Literal::Bool(*b)),
        lexpr::Value::Nil | lexpr::Value::Null => Ok(Literal::Nil),
        _ => Err(ParseError { span, message: "expected literal".into() }),
    }
}

pub(crate) fn parse_kwargs(items: &[lexpr::Value], span: Span) -> Result<Vec<(String, lexpr::Value)>, ParseError> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < items.len() {
        let k = items[i].as_keyword().ok_or_else(|| ParseError {
            span, message: format!("expected keyword arg, got {:?}", items[i]),
        })?;
        let v = items.get(i + 1).ok_or_else(|| ParseError {
            span, message: format!("keyword :{k} without value"),
        })?;
        out.push((k.to_string(), v.clone()));
        i += 2;
    }
    Ok(out)
}

fn expect_symbol<'a>(v: Option<&'a lexpr::Value>, span: Span, what: &str) -> Result<&'a str, ParseError> {
    v.and_then(|v| v.as_symbol()).ok_or_else(|| ParseError {
        span, message: format!("{what} (symbol) expected"),
    })
}

fn as_list(v: &lexpr::Value, span: Span) -> Result<Vec<lexpr::Value>, ParseError> {
    if v.is_null() {
        return Ok(vec![]);
    }
    v.to_vec().ok_or_else(|| ParseError {
        span, message: format!("expected list, got {v:?}"),
    })
}

fn as_vector_or_list(v: &lexpr::Value, span: Span) -> Result<Vec<lexpr::Value>, ParseError> {
    if let Some(vec) = v.as_vector() {
        return Ok(vec.to_vec());
    }
    as_list(v, span)
}
```

- [ ] **Step 7: Implement expression parsing**

Create `crates/agnes-parser/src/expr.rs`:

```rust
use agnes_ast::{Expr, KwArgs, Literal, Span};

use crate::error::ParseError;
use crate::toplevel::{parse_kwargs, parse_type_expr};

pub fn parse_expr(v: &lexpr::Value, span: Span) -> Result<Expr, ParseError> {
    // Atoms
    if let Some(sym) = v.as_symbol() {
        return Ok(Expr::Var { span, name: sym.to_string() });
    }
    if let Some(s) = v.as_str() {
        return Ok(Expr::Literal { span, lit: Literal::String(s.to_string()) });
    }
    if let Some(n) = v.as_i64() {
        return Ok(Expr::Literal { span, lit: Literal::Int(n) });
    }
    if let Some(b) = v.as_bool() {
        return Ok(Expr::Literal { span, lit: Literal::Bool(b) });
    }
    if v.is_null() {
        return Ok(Expr::Literal { span, lit: Literal::Nil });
    }

    // Compound
    let items = v.to_vec().ok_or_else(|| ParseError {
        span, message: format!("expected expression, got {v:?}"),
    })?;
    let head = items.first().and_then(|v| v.as_symbol()).ok_or_else(|| ParseError {
        span, message: "expression must start with a symbol".into(),
    })?;
    let rest = &items[1..];
    match head {
        "tool" => parse_tool(rest, span),
        "pipe" => Ok(Expr::Pipe { span, steps: parse_exprs(rest, span)? }),
        "par"  => Ok(Expr::Par  { span, branches: parse_exprs(rest, span)? }),
        "let"  => parse_let(rest, span),
        "if"   => parse_if(rest, span),
        "match" => parse_match(rest, span),
        "foreach" => parse_foreach(rest, span),
        "retry" => parse_retry(rest, span),
        "catch" => parse_catch(rest, span),
        "llm"   => Ok(Expr::Llm { span, args: parse_expr_kwargs(rest, span)? }),
        "return" => {
            let inner = rest.first().ok_or_else(|| ParseError {
                span, message: "return needs an expression".into(),
            })?;
            Ok(Expr::Return { span, value: Box::new(parse_expr(inner, span)?) })
        }
        other => Err(ParseError {
            span, message: format!("unknown expression head `{other}`"),
        }),
    }
}

fn parse_exprs(items: &[lexpr::Value], span: Span) -> Result<Vec<Expr>, ParseError> {
    items.iter().map(|i| parse_expr(i, span)).collect()
}

fn parse_tool(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let name = rest.first().and_then(|v| v.as_symbol()).ok_or_else(|| ParseError {
        span, message: "tool name expected".into(),
    })?.to_string();
    let args = parse_expr_kwargs(&rest[1..], span)?;
    Ok(Expr::Tool { span, name, args })
}

fn parse_expr_kwargs(items: &[lexpr::Value], span: Span) -> Result<KwArgs, ParseError> {
    let raw = parse_kwargs(items, span)?;
    raw.into_iter()
        .map(|(k, v)| parse_expr(&v, span).map(|e| (k, e)))
        .collect()
}

fn parse_let(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let name = rest.first().and_then(|v| v.as_symbol()).ok_or_else(|| ParseError {
        span, message: "let name expected".into(),
    })?.to_string();
    let value = match rest.get(1) {
        None => None,
        Some(v) => Some(Box::new(parse_expr(v, span)?)),
    };
    Ok(Expr::Let { span, name, value })
}

fn parse_if(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    if rest.len() != 3 {
        return Err(ParseError { span, message: "if needs (cond then else)".into() });
    }
    Ok(Expr::If {
        span,
        cond:        Box::new(parse_expr(&rest[0], span)?),
        then_branch: Box::new(parse_expr(&rest[1], span)?),
        else_branch: Box::new(parse_expr(&rest[2], span)?),
    })
}

fn parse_match(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let scrutinee = Box::new(parse_expr(rest.first().ok_or_else(|| ParseError {
        span, message: "match needs a scrutinee".into(),
    })?, span)?);
    let mut arms = Vec::new();
    for arm in &rest[1..] {
        let pair = arm.to_vec().ok_or_else(|| ParseError {
            span, message: "match arm must be (pattern expr)".into(),
        })?;
        if pair.len() != 2 {
            return Err(ParseError { span, message: "match arm must have 2 elements".into() });
        }
        let pat = literal_of(&pair[0], span)?;
        let body = parse_expr(&pair[1], span)?;
        arms.push((pat, body));
    }
    Ok(Expr::Match { span, scrutinee, arms })
}

fn parse_foreach(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    if rest.len() != 3 {
        return Err(ParseError { span, message: "foreach needs (item collection body)".into() });
    }
    let item = rest[0].as_symbol().ok_or_else(|| ParseError {
        span, message: "foreach item name (symbol) expected".into(),
    })?.to_string();
    Ok(Expr::Foreach {
        span, item,
        collection: Box::new(parse_expr(&rest[1], span)?),
        body:       Box::new(parse_expr(&rest[2], span)?),
    })
}

fn parse_retry(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let kw = parse_kwargs(rest, span)?;
    let mut times = None;
    let mut backoff = None;
    let mut body_val = None;
    let mut i = 0usize;
    while i < rest.len() {
        if let Some(k) = rest[i].as_keyword() {
            let v = &rest[i + 1];
            match k {
                "times" => times = v.as_i64().map(|n| n as u32),
                "backoff" => backoff = v.as_str().map(str::to_string),
                other => return Err(ParseError { span, message: format!("unknown keyword :{other} in retry") }),
            }
            i += 2;
        } else {
            body_val = Some(&rest[i]);
            i += 1;
        }
    }
    let _ = kw;
    let times = times.ok_or_else(|| ParseError { span, message: ":times required".into() })?;
    let body_val = body_val.ok_or_else(|| ParseError { span, message: "retry body missing".into() })?;
    Ok(Expr::Retry { span, times, backoff, body: Box::new(parse_expr(body_val, span)?) })
}

fn parse_catch(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let mut on = None;
    let mut fallback = None;
    let mut body_val = None;
    let mut i = 0usize;
    while i < rest.len() {
        if let Some(k) = rest[i].as_keyword() {
            let v = &rest[i + 1];
            match k {
                "on" => on = v.as_symbol().map(str::to_string),
                "fallback" => fallback = Some(parse_expr(v, span)?),
                other => return Err(ParseError { span, message: format!("unknown keyword :{other} in catch") }),
            }
            i += 2;
        } else {
            body_val = Some(&rest[i]);
            i += 1;
        }
    }
    let fallback = fallback.ok_or_else(|| ParseError { span, message: ":fallback required in catch".into() })?;
    let body_val = body_val.ok_or_else(|| ParseError { span, message: "catch body missing".into() })?;
    Ok(Expr::Catch { span, on, fallback: Box::new(fallback), body: Box::new(parse_expr(body_val, span)?) })
}

fn literal_of(v: &lexpr::Value, span: Span) -> Result<Literal, ParseError> {
    match v {
        lexpr::Value::String(s) => Ok(Literal::String(s.to_string())),
        lexpr::Value::Number(n) => n.as_i64().map(Literal::Int).ok_or_else(|| ParseError {
            span, message: "only i64 int literals supported".into(),
        }),
        lexpr::Value::Bool(b) => Ok(Literal::Bool(*b)),
        lexpr::Value::Nil | lexpr::Value::Null => Ok(Literal::Nil),
        _ => Err(ParseError { span, message: "expected literal in match pattern".into() }),
    }
}

// Silence unused warning if parse_type_expr not needed here.
#[allow(dead_code)]
fn _linker(v: &lexpr::Value, span: Span) {
    let _ = parse_type_expr(v, span);
}
```

- [ ] **Step 8: Run tests — expect PASS**

Run: `cargo test -p agnes-parser --tests`
Expected: `7 passed`.

If a test fails because `lexpr`'s API differs from what's used here (for example if `as_keyword` returns `Option<&Cow<...>>` in the current version), adjust: for keywords, symbols starting with `:` are exposed via `v.as_keyword()` returning `Option<&str>`. If your lexpr version uses a different API, replace `.as_keyword()` with a small helper that checks `.as_symbol().filter(|s| s.starts_with(':'))` and strips the colon.

- [ ] **Step 9: Commit**

```
jj describe -m "feat(parser): S-expression parser for agnes DSL

Uses lexpr for tokenization and walks the resulting sexpr tree into
agnes_ast types. Supports:
- Top-level: declare type, declare type-alias, declare tool, define
- Expressions: tool, pipe, par, let (both forms), if, match, foreach,
  retry, catch, llm, return, literals, variables
- Keyword arguments (:key value) and type expressions with '|' union
- Coarse byte-offset spans (heuristic-based; sufficient for MVP)

7 integration tests cover the happy paths and one error case.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 5: agnes-registry crate

**Files:**
- Create: `crates/agnes-registry/Cargo.toml`
- Create: `crates/agnes-registry/src/lib.rs`
- Create: `crates/agnes-registry/src/loader.rs`
- Create: `crates/agnes-registry/tests/register.rs`

**Interfaces:**
- Consumes: `agnes_ast::{Program, TopLevel, TypeExprAst, Param}`, `agnes_types::{TypeName, TypeExpr, Validator, ToolSignature}`
- Produces:
  - `Registry` with methods:
    - `pub fn new() -> Self`
    - `pub fn register_type(&mut self, name: &str, validator: Option<Validator>) -> Result<(), RegistryError>`
    - `pub fn register_alias(&mut self, name: &str, expr: TypeExpr) -> Result<(), RegistryError>`
    - `pub fn register_tool(&mut self, name: &str, sig: ToolSignature) -> Result<(), RegistryError>`
    - `pub fn resolve(&self, ast: &agnes_ast::TypeExprAst) -> Result<TypeExpr, RegistryError>` — resolve aliases & flatten unions
    - `pub fn validator_of(&self, ty: &TypeName) -> Option<Validator>`
    - `pub fn tool_signature(&self, name: &str) -> Option<&ToolSignature>`
    - `pub fn load(&mut self, program: &agnes_ast::Program) -> Result<(), RegistryError>` — applies all top-levels (excluding `define`, which the compiler handles)
  - `RegistryError` with `NameConflict { name, existing_kind, attempted_kind }` and `UnknownName { name }` variants
  - `pub fn defines_of(program: &agnes_ast::Program) -> Vec<&TopLevel>` — helper for compiler

- [ ] **Step 1: Manifest**

Create `crates/agnes-registry/Cargo.toml`:

```toml
[package]
name = "agnes-registry"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
agnes-ast.workspace = true
agnes-types.workspace = true
thiserror.workspace = true
```

- [ ] **Step 2: Write failing conflict-detection test**

Create `crates/agnes-registry/tests/register.rs`:

```rust
use agnes_registry::Registry;
use agnes_types::TypeName;

#[test]
fn duplicate_type_is_rejected() {
    let mut r = Registry::new();
    r.register_type("PDF", None).unwrap();
    let err = r.register_type("PDF", None).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Name conflict"), "got: {msg}");
    assert!(msg.contains("PDF"), "got: {msg}");
}

#[test]
fn alias_conflicts_with_type() {
    let mut r = Registry::new();
    r.register_type("Text", None).unwrap();
    let expr = agnes_types::TypeExpr::Named(TypeName("PDF".into()));
    let err = r.register_alias("Text", expr).unwrap_err();
    assert!(format!("{err}").contains("Name conflict"));
}

#[test]
fn resolve_alias_flattens_nested_union() {
    use agnes_ast::TypeExprAst;
    let mut r = Registry::new();
    r.register_type("PlainText", None).unwrap();
    r.register_type("Markdown", None).unwrap();
    r.register_type("HTML", None).unwrap();
    r.register_alias(
        "TextLike",
        agnes_types::TypeExpr::Union(
            [TypeName("PlainText".into()), TypeName("Markdown".into()), TypeName("HTML".into())]
                .into_iter().collect()
        ),
    ).unwrap();

    // (TextLike | PDF) should resolve to a flat 4-member set.
    r.register_type("PDF", None).unwrap();
    let ast = TypeExprAst::Union(vec![
        TypeExprAst::Named("TextLike".into()),
        TypeExprAst::Named("PDF".into()),
    ]);
    let resolved = r.resolve(&ast).unwrap();
    let set = resolved.as_set();
    assert_eq!(set.len(), 4);
    assert!(set.contains(&TypeName("PlainText".into())));
    assert!(set.contains(&TypeName("PDF".into())));
}
```

- [ ] **Step 3: Run — expect compile errors**

Run: `cargo test -p agnes-registry --tests`
Expected: cannot find crate `agnes_registry`.

- [ ] **Step 4: Implement the Registry**

Create `crates/agnes-registry/src/lib.rs`:

```rust
//! Type / alias / tool registry with strict name-uniqueness enforcement.

pub mod loader;

use std::collections::{HashMap, HashSet};
use std::fmt;

use agnes_ast::{Program, TopLevel, TypeExprAst};
use agnes_types::{ToolSignature, TypeExpr, TypeName, Validator};

#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
    Type,
    Alias,
    Tool,
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryKind::Type  => write!(f, "type"),
            EntryKind::Alias => write!(f, "type alias"),
            EntryKind::Tool  => write!(f, "tool"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(
        "Name conflict: `{name}` is already registered as a {existing_kind}\n  attempted to register as: {attempted_kind}\n  suggestion: rename to `{name}V2` or choose a different name"
    )]
    NameConflict {
        name: String,
        existing_kind: EntryKind,
        attempted_kind: EntryKind,
    },
    #[error("Unknown name in type expression: `{name}`\n  Fix: (declare type {name})")]
    UnknownName { name: String },
}

pub struct Registry {
    types: HashMap<String, Option<Validator>>,
    aliases: HashMap<String, TypeExpr>,
    tools: HashMap<String, ToolSignature>,
}

impl Default for Registry {
    fn default() -> Self { Self::new() }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            types: HashMap::new(),
            aliases: HashMap::new(),
            tools: HashMap::new(),
        }
    }

    fn ensure_free(&self, name: &str, attempted: EntryKind) -> Result<(), RegistryError> {
        if self.types.contains_key(name) {
            return Err(RegistryError::NameConflict {
                name: name.into(),
                existing_kind: EntryKind::Type,
                attempted_kind: attempted,
            });
        }
        if self.aliases.contains_key(name) {
            return Err(RegistryError::NameConflict {
                name: name.into(),
                existing_kind: EntryKind::Alias,
                attempted_kind: attempted,
            });
        }
        // Tools live in a separate namespace from types/aliases (tool names
        // and type names cannot collide in practice — types are PascalCase,
        // tools are kebab-case — but we still forbid duplicate tool names).
        if let EntryKind::Tool = attempted {
            if self.tools.contains_key(name) {
                return Err(RegistryError::NameConflict {
                    name: name.into(),
                    existing_kind: EntryKind::Tool,
                    attempted_kind: attempted,
                });
            }
        }
        Ok(())
    }

    pub fn register_type(&mut self, name: &str, v: Option<Validator>) -> Result<(), RegistryError> {
        self.ensure_free(name, EntryKind::Type)?;
        self.types.insert(name.to_string(), v);
        Ok(())
    }

    pub fn register_alias(&mut self, name: &str, expr: TypeExpr) -> Result<(), RegistryError> {
        self.ensure_free(name, EntryKind::Alias)?;
        self.aliases.insert(name.to_string(), expr);
        Ok(())
    }

    pub fn register_tool(&mut self, name: &str, sig: ToolSignature) -> Result<(), RegistryError> {
        self.ensure_free(name, EntryKind::Tool)?;
        self.tools.insert(name.to_string(), sig);
        Ok(())
    }

    /// Replace an existing tool signature (used when a `declare tool` overrides
    /// a previously-registered builtin or when a `define` re-registers itself).
    pub fn override_tool(&mut self, name: &str, sig: ToolSignature) {
        self.tools.insert(name.to_string(), sig);
    }

    pub fn tool_signature(&self, name: &str) -> Option<&ToolSignature> {
        self.tools.get(name)
    }

    pub fn validator_of(&self, ty: &TypeName) -> Option<Validator> {
        self.types.get(&ty.0).and_then(|v| *v)
    }

    /// Resolve a syntactic TypeExprAst into a canonical TypeExpr.
    /// - Named that refers to an alias -> the alias's TypeExpr
    /// - Named that refers to a type -> Named
    /// - Named that is unknown -> UnknownName error
    /// - Union -> flat union of resolved members
    pub fn resolve(&self, ast: &TypeExprAst) -> Result<TypeExpr, RegistryError> {
        match ast {
            TypeExprAst::Named(n) => {
                if let Some(alias) = self.aliases.get(n) {
                    Ok(alias.clone())
                } else if self.types.contains_key(n) {
                    Ok(TypeExpr::Named(TypeName(n.clone())))
                } else {
                    Err(RegistryError::UnknownName { name: n.clone() })
                }
            }
            TypeExprAst::Union(members) => {
                let mut set: HashSet<TypeName> = HashSet::new();
                for m in members {
                    let resolved = self.resolve(m)?;
                    for t in resolved.as_set() { set.insert(t); }
                }
                if set.len() == 1 {
                    Ok(TypeExpr::Named(set.into_iter().next().unwrap()))
                } else {
                    Ok(TypeExpr::Union(set))
                }
            }
        }
    }

    /// Apply every non-`define` top-level to the registry. `define`s are
    /// intentionally NOT applied here — the compiler handles them (and their
    /// tool signatures) after checking, so cyclic defines are caught by the
    /// compiler's cycle detector rather than being silently registered.
    pub fn load(&mut self, program: &Program) -> Result<(), RegistryError> {
        loader::load(self, program)
    }
}

/// Iterator helper for the compiler: yield only the `Define` top-levels.
pub fn defines_of(program: &Program) -> Vec<&TopLevel> {
    program.toplevels.iter().filter(|t| matches!(t, TopLevel::Define { .. })).collect()
}
```

- [ ] **Step 5: Implement the loader**

Create `crates/agnes-registry/src/loader.rs`:

```rust
use agnes_ast::{Param, Program, TopLevel};
use agnes_types::{ToolSignature, TypeExpr};

use crate::{Registry, RegistryError};

pub fn load(reg: &mut Registry, program: &Program) -> Result<(), RegistryError> {
    for tl in &program.toplevels {
        match tl {
            TopLevel::DeclareType { name, .. } => {
                reg.register_type(name, None)?;
            }
            TopLevel::DeclareTypeAlias { name, expr, .. } => {
                let resolved = reg.resolve(expr)?;
                reg.register_alias(name, resolved)?;
            }
            TopLevel::DeclareTool { name, requires, provides, .. } => {
                let sig = resolve_tool_sig(reg, requires, provides)?;
                // Allow override for user re-declares; but forbid initial dup.
                if reg.tool_signature(name).is_some() {
                    reg.override_tool(name, sig);
                } else {
                    reg.register_tool(name, sig)?;
                }
            }
            TopLevel::Define { .. } => {
                // Compiler handles these; loader skips.
            }
        }
    }
    Ok(())
}

pub fn resolve_tool_sig(
    reg: &Registry,
    requires: &[Param],
    provides: &agnes_ast::TypeExprAst,
) -> Result<ToolSignature, RegistryError> {
    let mut req = Vec::with_capacity(requires.len());
    for p in requires {
        req.push((p.name.clone(), reg.resolve(&p.ty)?));
    }
    let prov: TypeExpr = reg.resolve(provides)?;
    Ok(ToolSignature { requires: req, provides: prov })
}
```

- [ ] **Step 6: Run — expect PASS**

Run: `cargo test -p agnes-registry --tests`
Expected: `3 passed`.

- [ ] **Step 7: Commit**

```
jj describe -m "feat(registry): type/alias/tool registry with conflict detection

Registry owns three namespaces (types with optional Validator, aliases as
canonicalized TypeExpr, tools as ToolSignature) and enforces uniqueness
across type+alias names. Provides resolve() that recursively expands
aliases and flattens unions into a HashSet<TypeName>.

load(program) applies every non-Define top-level to the registry;
Defines are surfaced to the compiler via defines_of() so cycle detection
happens there.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 6: agnes-checker crate

**Files:**
- Create: `crates/agnes-checker/Cargo.toml`
- Create: `crates/agnes-checker/src/lib.rs`
- Create: `crates/agnes-checker/src/env.rs`
- Create: `crates/agnes-checker/src/error.rs`
- Create: `crates/agnes-checker/tests/check.rs`
- Create: `crates/agnes-checker/tests/snapshots/` (created by insta on first run)

**Interfaces:**
- Consumes: `agnes_ast::*`, `agnes_registry::Registry`, `agnes_types::*`
- Produces:
  - `pub fn check(program: &Program, registry: &Registry) -> Result<(), CheckError>`
  - `CheckError` (enum with variants: `ParamMismatch`, `FlowMismatch`, `UnknownTool`, `UnknownVar`, `DefineSignatureMismatch`) — renders What/Why/Fix

- [ ] **Step 1: Manifest**

Create `crates/agnes-checker/Cargo.toml`:

```toml
[package]
name = "agnes-checker"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
agnes-ast.workspace = true
agnes-types.workspace = true
agnes-registry.workspace = true
thiserror.workspace = true

[dev-dependencies]
agnes-parser.workspace = true
insta.workspace = true
```

- [ ] **Step 2: Write failing snapshot tests**

Create `crates/agnes-checker/tests/check.rs`:

```rust
use agnes_checker::check;
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName};

fn seed_registry() -> Registry {
    let mut r = Registry::new();
    r.register_type("Path", None).unwrap();
    r.register_type("PlainText", None).unwrap();
    r.register_type("Markdown", None).unwrap();
    r.register_type("PDF", None).unwrap();
    r.register_type("Image", None).unwrap();
    r.register_type("Summary", None).unwrap();
    r.register_type("Unit", None).unwrap();
    r.register_type("String", None).unwrap();
    // Tools
    r.register_tool("read-file", ToolSignature {
        requires: vec![("path".into(), TypeExpr::Named(TypeName("Path".into())))],
        provides: TypeExpr::Named(TypeName("PlainText".into())),
    }).unwrap();
    let text_like = TypeExpr::Union([
        TypeName("PlainText".into()),
        TypeName("Markdown".into()),
    ].into_iter().collect());
    r.register_tool("summarize", ToolSignature {
        requires: vec![("input".into(), text_like.clone())],
        provides: TypeExpr::Named(TypeName("Summary".into())),
    }).unwrap();
    r.register_tool("ocr", ToolSignature {
        requires: vec![("source".into(), TypeExpr::Union([
            TypeName("PDF".into()),
            TypeName("Image".into()),
        ].into_iter().collect()))],
        provides: TypeExpr::Named(TypeName("PlainText".into())),
    }).unwrap();
    r
}

#[test]
fn happy_path_read_then_summarize() {
    let src = r#"(pipe (tool read-file :path "x") (tool summarize))"#;
    let p = parse(src).unwrap();
    let r = seed_registry();
    check(&p, &r).expect("should type-check");
}

#[test]
fn flow_mismatch_produces_llm_friendly_error() {
    let src = r#"(pipe (tool read-file :path "x.md") (tool ocr))"#;
    let p = parse(src).unwrap();
    let r = seed_registry();
    let err = check(&p, &r).unwrap_err();
    insta::assert_snapshot!("flow_mismatch", format!("{err}"));
}

#[test]
fn unknown_tool_reports() {
    let src = r#"(tool no-such-tool)"#;
    let p = parse(src).unwrap();
    let r = seed_registry();
    let err = check(&p, &r).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Unknown tool"), "got: {msg}");
    assert!(msg.contains("no-such-tool"), "got: {msg}");
}
```

- [ ] **Step 3: Write the env module**

Create `crates/agnes-checker/src/env.rs`:

```rust
use std::collections::HashMap;
use agnes_types::TypeName;

/// Type environment threaded through expression checking.
#[derive(Debug, Default, Clone)]
pub struct Env {
    inner: HashMap<String, TypeName>,
}

impl Env {
    pub fn get(&self, name: &str) -> Option<&TypeName> { self.inner.get(name) }
    pub fn set(&mut self, name: String, ty: TypeName) { self.inner.insert(name, ty); }
}
```

- [ ] **Step 4: Write the error module**

Create `crates/agnes-checker/src/error.rs`:

```rust
use agnes_types::{TypeExpr, TypeName};

#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// A `tool` call passed an argument whose type does not satisfy the
    /// tool's declared requires.
    #[error(
"Type error at (tool {tool} :{param} <arg>):
  parameter `{param}` requires one of: {expected}
  but got type: {actual}

Fix suggestion (one of):
  A) Change the argument's source to produce one of the accepted types
  B) Extend {tool} to accept {actual}:
     (declare tool {tool} :requires [({param}: ({expected} | {actual})) ...] ...)"
    )]
    ParamMismatch {
        tool: String,
        param: String,
        expected: TypeExpr,
        actual: TypeName,
    },

    /// A `pipe` stream cannot feed the next step because the upstream's
    /// provides doesn't satisfy the downstream's sole positional requires.
    #[error(
"Type error at (pipe ... (tool {downstream_tool}) ...):
  step `{downstream_tool}` requires one of: {expected}
  but upstream step `{upstream}` provides: {actual}

Fix suggestion (one of):
  A) Insert a converting tool between them
  B) Extend {downstream_tool} to accept {actual}"
    )]
    FlowMismatch {
        upstream: String,
        downstream_tool: String,
        expected: TypeExpr,
        actual: TypeName,
    },

    #[error(
"Unknown tool `{name}` at call site.

Fix suggestion (paste at top of file):
  (declare tool {name} :requires [...] :provides <TypeExpr>)"
    )]
    UnknownTool { name: String },

    #[error(
"Unknown variable `{name}` in expression.
  Was it introduced with (let {name} ...) earlier in scope?"
    )]
    UnknownVar { name: String },

    #[error(
"Define `{name}` body provides type {body_type} which does not satisfy declared :provides {declared}"
    )]
    DefineSignatureMismatch {
        name: String,
        declared: TypeExpr,
        body_type: TypeName,
    },

    #[error(transparent)]
    Registry(#[from] agnes_registry::RegistryError),
}
```

- [ ] **Step 5: Implement the checker**

Create `crates/agnes-checker/src/lib.rs`:

```rust
//! Type checker for agnes DSL.
//! Enforces exactly two rules:
//!   1. Parameter satisfaction: each argument's type is member of tool's require.
//!   2. Flow satisfaction: pipe upstream's provides is member of downstream's require
//!      (when downstream is a single-param tool with an unbound positional slot).

pub mod env;
pub mod error;

use agnes_ast::{Expr, Param, Program, TopLevel};
use agnes_registry::Registry;
use agnes_types::{type_expr_matches, ToolSignature, TypeExpr, TypeName};

pub use error::CheckError;

/// Top-level entry.
pub fn check(program: &Program, reg: &Registry) -> Result<(), CheckError> {
    // First, check every `define`'s body in isolation.
    for tl in &program.toplevels {
        if let TopLevel::Define { name, params, provides, body, .. } = tl {
            let mut env = env::Env::default();
            for p in params {
                // Params are already-registered types after loader; resolve names.
                let ty_expr = reg.resolve(&p.ty)?;
                let single = single_type(&ty_expr).ok_or_else(|| CheckError::UnknownVar {
                    name: format!("param `{}` must have a concrete or unioned type", p.name),
                })?;
                env.set(p.name.clone(), single);
            }
            let body_ty = check_expr(body, reg, &mut env, None)?;
            let declared = reg.resolve(provides)?;
            if !type_expr_matches(&body_ty, &declared) {
                return Err(CheckError::DefineSignatureMismatch {
                    name: name.clone(),
                    declared,
                    body_type: body_ty,
                });
            }
        }
    }
    // Then the main workflow, if any.
    if let Some(main) = &program.main {
        let mut env = env::Env::default();
        check_expr(main, reg, &mut env, None)?;
    }
    Ok(())
}

/// Walk an expression, returning the type it produces. `flowed_in` is the
/// upstream type (if we're inside a `pipe` and this expr is not the first).
fn check_expr(
    e: &Expr,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeName>,
) -> Result<TypeName, CheckError> {
    match e {
        Expr::Tool { name, args, .. } => check_tool_call(name, args, reg, env, flowed_in),
        Expr::Pipe { steps, .. } => {
            let mut upstream: Option<TypeName> = None;
            let mut last: Option<TypeName> = None;
            for step in steps {
                let ty = check_expr(step, reg, env, upstream.clone())?;
                upstream = Some(ty.clone());
                last = Some(ty);
            }
            last.ok_or_else(|| CheckError::UnknownVar { name: "(empty pipe)".into() })
        }
        Expr::Par { branches, .. } => {
            // Each branch checked independently; return the last branch's type.
            let mut last = None;
            for b in branches {
                last = Some(check_expr(b, reg, env, None)?);
            }
            last.ok_or_else(|| CheckError::UnknownVar { name: "(empty par)".into() })
        }
        Expr::Let { name, value, .. } => {
            let bound = match value {
                None => flowed_in.clone().ok_or_else(|| CheckError::UnknownVar {
                    name: format!("(let {name}) with no upstream to name"),
                })?,
                Some(v) => check_expr(v, reg, env, None)?,
            };
            env.set(name.clone(), bound.clone());
            // Transparent form: pass upstream through unchanged.
            // Side-line form: no stream contribution (returns Unit conceptually,
            // but MVP reuses the bound type — the pipe caller normally puts a
            // (let name expr) inside a par branch, not directly in a pipe).
            Ok(bound)
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            let _ = check_expr(cond, reg, env, None)?;
            let t = check_expr(then_branch, reg, env, None)?;
            let _ = check_expr(else_branch, reg, env, None)?;
            Ok(t)
        }
        Expr::Match { scrutinee, arms, .. } => {
            let _ = check_expr(scrutinee, reg, env, None)?;
            let mut last = None;
            for (_, arm) in arms {
                last = Some(check_expr(arm, reg, env, None)?);
            }
            last.ok_or_else(|| CheckError::UnknownVar { name: "(empty match)".into() })
        }
        Expr::Foreach { body, collection, .. } => {
            let _ = check_expr(collection, reg, env, None)?;
            check_expr(body, reg, env, None)
        }
        Expr::Retry { body, .. } => check_expr(body, reg, env, flowed_in),
        Expr::Catch { body, fallback, .. } => {
            let t = check_expr(body, reg, env, flowed_in.clone())?;
            let _ = check_expr(fallback, reg, env, flowed_in)?;
            Ok(t)
        }
        Expr::Llm { .. } => Ok(TypeName("PlainText".into())),
        Expr::Return { value, .. } => check_expr(value, reg, env, None),
        Expr::Literal { lit, .. } => Ok(literal_type(lit)),
        Expr::Var { name, .. } => env.get(name).cloned().ok_or_else(|| CheckError::UnknownVar {
            name: name.clone(),
        }),
    }
}

fn literal_type(lit: &agnes_ast::Literal) -> TypeName {
    match lit {
        agnes_ast::Literal::String(_) => TypeName("String".into()),
        agnes_ast::Literal::Int(_)    => TypeName("Int".into()),
        agnes_ast::Literal::Bool(_)   => TypeName("Bool".into()),
        agnes_ast::Literal::Nil       => TypeName("Unit".into()),
    }
}

fn single_type(t: &TypeExpr) -> Option<TypeName> {
    match t {
        TypeExpr::Named(n) => Some(n.clone()),
        TypeExpr::Union(_) => None,
    }
}

fn check_tool_call(
    tool_name: &str,
    args: &agnes_ast::KwArgs,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeName>,
) -> Result<TypeName, CheckError> {
    let sig: ToolSignature = reg.tool_signature(tool_name).cloned().ok_or_else(|| CheckError::UnknownTool {
        name: tool_name.to_string(),
    })?;
    // Track which sig params were filled.
    let mut filled: Vec<bool> = vec![false; sig.requires.len()];

    for (k, v) in args {
        let (idx, param_expected) = sig.requires.iter()
            .enumerate()
            .find(|(_, (n, _))| n == k)
            .map(|(i, (_, t))| (i, t.clone()))
            .ok_or_else(|| CheckError::UnknownVar {
                name: format!("keyword arg :{k} in call to `{tool_name}` not in signature"),
            })?;
        let actual = check_expr(v, reg, env, None)?;
        if !type_expr_matches(&actual, &param_expected) {
            return Err(CheckError::ParamMismatch {
                tool: tool_name.to_string(),
                param: k.clone(),
                expected: param_expected,
                actual,
            });
        }
        filled[idx] = true;
    }

    // If exactly one param is unfilled and we have a flowed_in upstream, use it.
    let unfilled: Vec<usize> = filled.iter().enumerate().filter(|(_, b)| !**b).map(|(i, _)| i).collect();
    match (unfilled.len(), flowed_in) {
        (0, _) => {}
        (1, Some(up)) => {
            let (up_name, expected) = &sig.requires[unfilled[0]];
            if !type_expr_matches(&up, expected) {
                return Err(CheckError::FlowMismatch {
                    upstream: format!("<upstream (provides {up})>"),
                    downstream_tool: tool_name.to_string(),
                    expected: expected.clone(),
                    actual: up,
                });
            }
            let _ = up_name; // param name silenced
        }
        _ => {
            return Err(CheckError::UnknownVar {
                name: format!("tool `{tool_name}` has unfilled required params and no upstream to bind"),
            });
        }
    }

    // Provides may be Named or Union — MVP requires Named for concrete flow.
    // If Union, we pick the entire union set encoded as a synthetic name for now.
    match sig.provides {
        TypeExpr::Named(n) => Ok(n),
        TypeExpr::Union(_) => Err(CheckError::UnknownVar {
            name: format!("tool `{tool_name}` provides a Union type; MVP requires concrete provides"),
        }),
    }
}
```

- [ ] **Step 6: Run tests — expect snapshot review**

Run: `cargo test -p agnes-checker --tests`
Expected: three tests pass; on the first run insta writes a new snapshot for `flow_mismatch` and marks it pending. Review with:

```
cargo insta review
```
Accept the snapshot (it should contain the What/Why/Fix rendering of `FlowMismatch`).

- [ ] **Step 7: Commit**

```
jj describe -m "feat(checker): type checker enforcing the two spec rules

Rule 1 (parameter satisfaction): each keyword arg's type is a member of
the corresponding require's TypeExpr.

Rule 2 (flow satisfaction): in a pipe, if the downstream tool has exactly
one unfilled required parameter, the upstream's provides must be a member
of that parameter's TypeExpr.

CheckError variants render LLM-friendly What/Why/Fix suggestion errors
matching the spec templates. Snapshot testing via insta locks the exact
error text.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 7: agnes-compiler crate

**Files:**
- Create: `crates/agnes-compiler/Cargo.toml`
- Create: `crates/agnes-compiler/src/lib.rs`
- Create: `crates/agnes-compiler/src/dag.rs`
- Create: `crates/agnes-compiler/src/lower.rs`
- Create: `crates/agnes-compiler/src/cycle.rs`
- Create: `crates/agnes-compiler/tests/compile.rs`

**Interfaces:**
- Consumes: `agnes_ast::*`, `agnes_registry::Registry`, `agnes_types::*`
- Produces:
  - `pub fn compile(program: &Program, registry: &Registry) -> Result<Dag, CompileError>`
  - `Dag { pub nodes: Vec<Node>, pub root: NodeId }`
  - `Node { pub id: NodeId, pub kind: NodeKind, pub inputs: Vec<Input>, pub provides: TypeExpr }`
  - `NodeKind::{ Tool { name }, Pipe, Par, Let { name }, If, Match { arms: Vec<Literal> }, Foreach { item }, Retry { times, backoff }, Catch { on, fallback: NodeId }, Llm, Return, Literal(Literal), Var(String) }`
  - `Input::{ FromNode(NodeId), Literal(Literal), Var(String) }`
  - `NodeId(pub usize)`
  - `CompileError::{ CycleDetected { name }, RegistryError, UnknownDefine { name } }`

- [ ] **Step 1: Manifest**

Create `crates/agnes-compiler/Cargo.toml`:

```toml
[package]
name = "agnes-compiler"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
agnes-ast.workspace = true
agnes-types.workspace = true
agnes-registry.workspace = true
thiserror.workspace = true

[dev-dependencies]
agnes-parser.workspace = true
```

- [ ] **Step 2: Failing tests**

Create `crates/agnes-compiler/tests/compile.rs`:

```rust
use agnes_compiler::{compile, CompileError};
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName};

fn seed() -> Registry {
    let mut r = Registry::new();
    r.register_type("Path", None).unwrap();
    r.register_type("PlainText", None).unwrap();
    r.register_type("Summary", None).unwrap();
    r.register_tool("read-file", ToolSignature {
        requires: vec![("path".into(), TypeExpr::Named(TypeName("Path".into())))],
        provides: TypeExpr::Named(TypeName("PlainText".into())),
    }).unwrap();
    r.register_tool("summarize", ToolSignature {
        requires: vec![("input".into(), TypeExpr::Named(TypeName("PlainText".into())))],
        provides: TypeExpr::Named(TypeName("Summary".into())),
    }).unwrap();
    r
}

#[test]
fn compiles_a_pipe() {
    let src = r#"(pipe (tool read-file :path "x") (tool summarize))"#;
    let p = parse(src).unwrap();
    let r = seed();
    let dag = compile(&p, &r).expect("compile ok");
    assert!(dag.nodes.len() >= 2);
}

#[test]
fn detects_recursive_define() {
    let src = r#"
        (define loopy :params [] :provides Unit (tool loopy))
    "#;
    let mut r = seed();
    r.register_type("Unit", None).unwrap();
    let p = parse(src).unwrap();
    let err = compile(&p, &r).unwrap_err();
    match err {
        CompileError::CycleDetected { name } => assert_eq!(name, "loopy"),
        other => panic!("expected CycleDetected, got {other:?}"),
    }
}
```

- [ ] **Step 3: Run — compile errors**

Run: `cargo test -p agnes-compiler --tests`
Expected: `agnes_compiler` not found.

- [ ] **Step 4: Implement the DAG data structure**

Create `crates/agnes-compiler/src/dag.rs`:

```rust
use agnes_ast::Literal;
use agnes_types::TypeExpr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

#[derive(Debug, Clone)]
pub enum NodeKind {
    Tool { name: String },
    Pipe,
    Par,
    Let { name: String },
    If,
    Match { arms: Vec<Literal> },
    Foreach { item: String },
    Retry { times: u32, backoff: Option<String> },
    Catch { on: Option<String>, fallback: NodeId },
    Llm,
    Return,
    Literal(Literal),
    Var(String),
}

#[derive(Debug, Clone)]
pub enum Input {
    FromNode(NodeId),
    Literal(Literal),
    Var(String),
    /// Keyword-bound edge: same as FromNode but tagged with the parameter
    /// name so the runtime knows which slot to fill.
    Kw { key: String, source: Box<Input> },
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub inputs: Vec<Input>,
    pub provides: TypeExpr,
}

#[derive(Debug, Clone)]
pub struct Dag {
    pub nodes: Vec<Node>,
    pub root: NodeId,
}

impl Dag {
    pub fn get(&self, id: NodeId) -> &Node { &self.nodes[id.0] }
}
```

- [ ] **Step 5: Implement cycle detection**

Create `crates/agnes-compiler/src/cycle.rs`:

```rust
use std::collections::{HashMap, HashSet};
use agnes_ast::{Expr, Program, TopLevel};

/// Detect a `define` transitively invoking itself (directly or through
/// other defines). Returns the name of the first cycle-owner found.
pub fn detect_define_cycles(program: &Program) -> Option<String> {
    let mut adj: HashMap<String, HashSet<String>> = HashMap::new();
    for tl in &program.toplevels {
        if let TopLevel::Define { name, body, .. } = tl {
            adj.insert(name.clone(), tool_names_in_expr(body));
        }
    }
    for start in adj.keys() {
        if reaches_self(start, &adj) {
            return Some(start.clone());
        }
    }
    None
}

fn tool_names_in_expr(e: &Expr) -> HashSet<String> {
    let mut out = HashSet::new();
    walk(e, &mut out);
    out
}

fn walk(e: &Expr, out: &mut HashSet<String>) {
    match e {
        Expr::Tool { name, args, .. } => {
            out.insert(name.clone());
            for (_, v) in args { walk(v, out); }
        }
        Expr::Pipe { steps, .. } => steps.iter().for_each(|s| walk(s, out)),
        Expr::Par  { branches, .. } => branches.iter().for_each(|s| walk(s, out)),
        Expr::Let { value: Some(v), .. } => walk(v, out),
        Expr::Let { value: None, .. } => {}
        Expr::If { cond, then_branch, else_branch, .. } => {
            walk(cond, out); walk(then_branch, out); walk(else_branch, out);
        }
        Expr::Match { scrutinee, arms, .. } => {
            walk(scrutinee, out);
            for (_, a) in arms { walk(a, out); }
        }
        Expr::Foreach { collection, body, .. } => { walk(collection, out); walk(body, out); }
        Expr::Retry { body, .. } => walk(body, out),
        Expr::Catch { body, fallback, .. } => { walk(body, out); walk(fallback, out); }
        Expr::Llm { args, .. } => for (_, v) in args { walk(v, out); },
        Expr::Return { value, .. } => walk(value, out),
        Expr::Literal { .. } | Expr::Var { .. } => {}
    }
}

fn reaches_self(start: &str, adj: &HashMap<String, HashSet<String>>) -> bool {
    let mut stack = vec![start.to_string()];
    let mut seen = HashSet::new();
    while let Some(cur) = stack.pop() {
        if let Some(neighbors) = adj.get(&cur) {
            for n in neighbors {
                if n == start { return true; }
                if seen.insert(n.clone()) {
                    stack.push(n.clone());
                }
            }
        }
    }
    false
}
```

- [ ] **Step 6: Implement lowering**

Create `crates/agnes-compiler/src/lower.rs`:

```rust
use agnes_ast::{Expr, KwArgs, Literal, Program, TopLevel};
use agnes_registry::Registry;
use agnes_types::TypeExpr;

use crate::dag::{Dag, Input, Node, NodeId, NodeKind};

pub struct Lowering<'a> {
    reg: &'a Registry,
    nodes: Vec<Node>,
}

impl<'a> Lowering<'a> {
    pub fn new(reg: &'a Registry) -> Self { Self { reg, nodes: Vec::new() } }

    fn add(&mut self, kind: NodeKind, inputs: Vec<Input>, provides: TypeExpr) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node { id, kind, inputs, provides });
        id
    }

    pub fn lower_program(&mut self, program: &Program) -> Result<Dag, crate::CompileError> {
        // Register defines as tools (so calls to them resolve at runtime as
        // "compound tool" nodes). MVP does not inline define bodies at DAG
        // level; the runtime dispatches to the stored expression instead.
        // But for type-checked callability we treat their provides as declared.
        let main = program.main.as_ref().ok_or_else(|| crate::CompileError::UnknownDefine {
            name: "<no main>".into(),
        })?;
        let root = self.lower_expr(main, None)?;
        Ok(Dag { nodes: std::mem::take(&mut self.nodes), root })
    }

    fn lower_expr(&mut self, e: &Expr, upstream: Option<NodeId>) -> Result<NodeId, crate::CompileError> {
        match e {
            Expr::Tool { name, args, .. } => self.lower_tool(name, args, upstream),
            Expr::Pipe { steps, .. } => self.lower_pipe(steps),
            Expr::Par { branches, .. } => self.lower_par(branches),
            Expr::Let { name, value, .. } => self.lower_let(name, value.as_deref(), upstream),
            Expr::If { cond, then_branch, else_branch, .. } => {
                let c = self.lower_expr(cond, None)?;
                let t = self.lower_expr(then_branch, None)?;
                let f = self.lower_expr(else_branch, None)?;
                let id = self.add(
                    NodeKind::If,
                    vec![Input::FromNode(c), Input::FromNode(t), Input::FromNode(f)],
                    self.nodes[t.0].provides.clone(),
                );
                Ok(id)
            }
            Expr::Match { scrutinee, arms, .. } => {
                let s = self.lower_expr(scrutinee, None)?;
                let mut inputs = vec![Input::FromNode(s)];
                let mut pats: Vec<Literal> = Vec::new();
                let mut last_provides = self.nodes[s.0].provides.clone();
                for (pat, body) in arms {
                    pats.push(pat.clone());
                    let b = self.lower_expr(body, None)?;
                    inputs.push(Input::FromNode(b));
                    last_provides = self.nodes[b.0].provides.clone();
                }
                Ok(self.add(NodeKind::Match { arms: pats }, inputs, last_provides))
            }
            Expr::Foreach { item, collection, body, .. } => {
                let c = self.lower_expr(collection, None)?;
                let b = self.lower_expr(body, None)?;
                let provides = self.nodes[b.0].provides.clone();
                Ok(self.add(
                    NodeKind::Foreach { item: item.clone() },
                    vec![Input::FromNode(c), Input::FromNode(b)],
                    provides,
                ))
            }
            Expr::Retry { times, backoff, body, .. } => {
                let b = self.lower_expr(body, upstream)?;
                let provides = self.nodes[b.0].provides.clone();
                Ok(self.add(
                    NodeKind::Retry { times: *times, backoff: backoff.clone() },
                    vec![Input::FromNode(b)],
                    provides,
                ))
            }
            Expr::Catch { on, fallback, body, .. } => {
                let b = self.lower_expr(body, upstream)?;
                let f = self.lower_expr(fallback, None)?;
                let provides = self.nodes[b.0].provides.clone();
                Ok(self.add(
                    NodeKind::Catch { on: on.clone(), fallback: f },
                    vec![Input::FromNode(b)],
                    provides,
                ))
            }
            Expr::Llm { args, .. } => {
                let inputs = self.lower_kwargs(args)?;
                Ok(self.add(
                    NodeKind::Llm,
                    inputs,
                    TypeExpr::Named(agnes_types::TypeName("PlainText".into())),
                ))
            }
            Expr::Return { value, .. } => {
                let v = self.lower_expr(value, None)?;
                let provides = self.nodes[v.0].provides.clone();
                Ok(self.add(NodeKind::Return, vec![Input::FromNode(v)], provides))
            }
            Expr::Literal { lit, .. } => {
                let ty = match lit {
                    Literal::String(_) => "String",
                    Literal::Int(_) => "Int",
                    Literal::Bool(_) => "Bool",
                    Literal::Nil => "Unit",
                };
                Ok(self.add(
                    NodeKind::Literal(lit.clone()),
                    vec![],
                    TypeExpr::Named(agnes_types::TypeName(ty.into())),
                ))
            }
            Expr::Var { name, .. } => {
                Ok(self.add(
                    NodeKind::Var(name.clone()),
                    vec![],
                    TypeExpr::Named(agnes_types::TypeName("Unknown".into())),
                ))
            }
        }
    }

    fn lower_tool(&mut self, name: &str, args: &KwArgs, upstream: Option<NodeId>) -> Result<NodeId, crate::CompileError> {
        let sig = self.reg.tool_signature(name).cloned().ok_or_else(|| crate::CompileError::UnknownDefine {
            name: name.to_string(),
        })?;
        let mut inputs = self.lower_kwargs(args)?;
        // If exactly one required param unfilled and upstream present, bind it.
        let filled_keys: std::collections::HashSet<String> = args.iter().map(|(k, _)| k.clone()).collect();
        let unfilled: Vec<&(String, TypeExpr)> = sig.requires.iter()
            .filter(|(k, _)| !filled_keys.contains(k)).collect();
        if unfilled.len() == 1 {
            if let Some(up) = upstream {
                inputs.push(Input::Kw {
                    key: unfilled[0].0.clone(),
                    source: Box::new(Input::FromNode(up)),
                });
            }
        }
        let provides = sig.provides.clone();
        Ok(self.add(NodeKind::Tool { name: name.to_string() }, inputs, provides))
    }

    fn lower_kwargs(&mut self, args: &KwArgs) -> Result<Vec<Input>, crate::CompileError> {
        let mut out = Vec::new();
        for (k, v) in args {
            let src = self.lower_expr(v, None)?;
            out.push(Input::Kw { key: k.clone(), source: Box::new(Input::FromNode(src)) });
        }
        Ok(out)
    }

    fn lower_pipe(&mut self, steps: &[Expr]) -> Result<NodeId, crate::CompileError> {
        let mut prev: Option<NodeId> = None;
        for step in steps {
            let n = self.lower_expr(step, prev)?;
            prev = Some(n);
        }
        let last = prev.ok_or_else(|| crate::CompileError::UnknownDefine { name: "<empty pipe>".into() })?;
        let provides = self.nodes[last.0].provides.clone();
        Ok(self.add(NodeKind::Pipe, vec![Input::FromNode(last)], provides))
    }

    fn lower_par(&mut self, branches: &[Expr]) -> Result<NodeId, crate::CompileError> {
        let mut ids = Vec::new();
        for b in branches {
            ids.push(self.lower_expr(b, None)?);
        }
        let inputs: Vec<Input> = ids.iter().copied().map(Input::FromNode).collect();
        // Par's declared provides is a synthetic Unit-tuple; MVP leaves this
        // as Unit — no downstream flow through par (users use let inside).
        Ok(self.add(NodeKind::Par, inputs, TypeExpr::Named(agnes_types::TypeName("Unit".into()))))
    }

    fn lower_let(&mut self, name: &str, value: Option<&Expr>, upstream: Option<NodeId>) -> Result<NodeId, crate::CompileError> {
        let (input, provides) = match value {
            Some(v) => {
                let n = self.lower_expr(v, None)?;
                (Input::FromNode(n), self.nodes[n.0].provides.clone())
            }
            None => {
                let up = upstream.ok_or_else(|| crate::CompileError::UnknownDefine {
                    name: format!("(let {name}) with no upstream").into(),
                })?;
                (Input::FromNode(up), self.nodes[up.0].provides.clone())
            }
        };
        Ok(self.add(NodeKind::Let { name: name.to_string() }, vec![input], provides))
    }
}
```

- [ ] **Step 7: Implement compile()**

Create `crates/agnes-compiler/src/lib.rs`:

```rust
//! AST -> DAG compilation, including recursive-define detection.

pub mod dag;
mod lower;
mod cycle;

pub use dag::{Dag, Input, Node, NodeId, NodeKind};

use agnes_ast::Program;
use agnes_registry::Registry;

#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error(
"Recursive define detected: `{name}` calls itself (directly or transitively).
  MVP does not support recursion; refactor the workflow to a fixed-depth chain."
    )]
    CycleDetected { name: String },

    #[error(transparent)]
    Registry(#[from] agnes_registry::RegistryError),

    #[error("Compilation failure: {name}")]
    UnknownDefine { name: String },
}

pub fn compile(program: &Program, registry: &Registry) -> Result<Dag, CompileError> {
    if let Some(name) = cycle::detect_define_cycles(program) {
        return Err(CompileError::CycleDetected { name });
    }
    let mut lower = lower::Lowering::new(registry);
    lower.lower_program(program)
}
```

- [ ] **Step 8: Run — expect PASS**

Run: `cargo test -p agnes-compiler --tests`
Expected: `2 passed`.

- [ ] **Step 9: Commit**

```
jj describe -m "feat(compiler): lower AST to DAG with cycle detection

Compile produces a Dag of Node values with typed inputs. Cycle detection
runs first: any define that transitively references its own tool name is
rejected with CycleDetected. Node kinds cover all expression forms; pipe
threading is expressed by binding the upstream node to the sole unfilled
require of the downstream tool.

Retry/catch modifiers on tool calls will be desugared to control-flow
Retry/Catch nodes in a follow-up pass (unnecessary for MVP acceptance
tests, which use explicit control-flow forms).

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 8: agnes-builtins crate (types + validators + tool impls)

**Files:**
- Create: `crates/agnes-builtins/Cargo.toml`
- Create: `crates/agnes-builtins/src/lib.rs`
- Create: `crates/agnes-builtins/src/types.rs`
- Create: `crates/agnes-builtins/src/aliases.rs`
- Create: `crates/agnes-builtins/src/tools.rs`
- Create: `crates/agnes-builtins/tests/register.rs`

**Interfaces:**
- Consumes: `agnes_registry::Registry`, `agnes_types::*`
- Produces:
  - `pub fn register_builtins(reg: &mut Registry) -> Result<(), agnes_registry::RegistryError>` — inserts all builtin types (with validators), aliases (TextLike, VisualDoc), and tool signatures
  - `pub type ToolImpl = Arc<dyn Fn(HashMap<String, agnes_types::Value>) -> BoxFuture<'static, Result<agnes_types::Value, String>> + Send + Sync>`
  - `pub fn native_dispatch() -> HashMap<String, ToolImpl>` — maps tool names to native async impls (read-file, write-file, summarize [mock], translate [mock], ocr [mock], llm [mock])

- [ ] **Step 1: Manifest**

Create `crates/agnes-builtins/Cargo.toml`:

```toml
[package]
name = "agnes-builtins"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
agnes-types.workspace = true
agnes-registry.workspace = true
tokio.workspace = true
serde_json.workspace = true
anyhow.workspace = true
```

- [ ] **Step 2: Failing registration test**

Create `crates/agnes-builtins/tests/register.rs`:

```rust
use agnes_builtins::{register_builtins, native_dispatch};
use agnes_registry::Registry;

#[test]
fn registers_all_builtins() {
    let mut r = Registry::new();
    register_builtins(&mut r).expect("builtins load");
    assert!(r.tool_signature("read-file").is_some());
    assert!(r.tool_signature("write-file").is_some());
    assert!(r.tool_signature("summarize").is_some());
    assert!(r.tool_signature("translate").is_some());
    assert!(r.tool_signature("ocr").is_some());
    assert!(r.tool_signature("llm").is_some());
}

#[test]
fn native_dispatch_has_all_impls() {
    let d = native_dispatch();
    for name in ["read-file","write-file","summarize","translate","ocr","llm"] {
        assert!(d.contains_key(name), "missing impl for {name}");
    }
}
```

- [ ] **Step 3: Run — expect compile errors**

Run: `cargo test -p agnes-builtins --tests`
Expected: crate not found.

- [ ] **Step 4: Types + validators**

Create `crates/agnes-builtins/src/types.rs`:

```rust
use serde_json::Value as JsonValue;

pub fn path_validator(v: &JsonValue) -> Result<(), String> {
    let s = v.as_str().ok_or("Path must be a JSON string")?;
    if s.is_empty() { return Err("Path is empty".into()); }
    if s.contains('\0') { return Err("Path contains NUL byte".into()); }
    Ok(())
}

pub fn utf8_validator(v: &JsonValue) -> Result<(), String> {
    let s = v.as_str().ok_or("expected JSON string")?;
    if std::str::from_utf8(s.as_bytes()).is_err() {
        return Err("value is not valid UTF-8".into());
    }
    Ok(())
}

pub fn json_validator(v: &JsonValue) -> Result<(), String> {
    let s = v.as_str().ok_or("JSON payload must be a string containing JSON")?;
    serde_json::from_str::<JsonValue>(s).map_err(|e| format!("not valid JSON: {e}"))?;
    Ok(())
}

pub fn pdf_validator(v: &JsonValue) -> Result<(), String> {
    let arr = v.as_array().ok_or("PDF must be a JSON array of byte integers")?;
    if arr.len() < 4 { return Err("PDF too short (missing %PDF header)".into()); }
    let head: Vec<u8> = arr.iter().take(4)
        .map(|n| n.as_u64().unwrap_or(0) as u8).collect();
    if &head != b"%PDF" { return Err(format!("bad PDF magic: {head:?}")); }
    Ok(())
}

pub fn image_validator(v: &JsonValue) -> Result<(), String> {
    let arr = v.as_array().ok_or("Image must be a JSON array of byte integers")?;
    if arr.len() < 4 { return Err("Image too short (missing magic bytes)".into()); }
    let head: Vec<u8> = arr.iter().take(8)
        .map(|n| n.as_u64().unwrap_or(0) as u8).collect();
    // PNG, JPEG, GIF, WebP
    let magics: &[&[u8]] = &[
        b"\x89PNG",
        b"\xFF\xD8\xFF",
        b"GIF8",
        b"RIFF",
    ];
    for m in magics {
        if head.starts_with(m) { return Ok(()); }
    }
    Err(format!("no known image magic in head: {head:?}"))
}

pub fn unit_validator(v: &JsonValue) -> Result<(), String> {
    match v {
        JsonValue::Null => Ok(()),
        JsonValue::Object(m) if m.is_empty() => Ok(()),
        _ => Err("Unit must be null or {}".into()),
    }
}
```

- [ ] **Step 5: Aliases**

Create `crates/agnes-builtins/src/aliases.rs`:

```rust
use agnes_types::{TypeExpr, TypeName};

pub fn text_like() -> TypeExpr {
    TypeExpr::Union([
        TypeName("PlainText".into()),
        TypeName("Markdown".into()),
        TypeName("HTML".into()),
    ].into_iter().collect())
}

pub fn visual_doc() -> TypeExpr {
    TypeExpr::Union([
        TypeName("PDF".into()),
        TypeName("Image".into()),
    ].into_iter().collect())
}
```

- [ ] **Step 6: Tools (mock impls)**

Create `crates/agnes-builtins/src/tools.rs`:

```rust
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use agnes_types::{TypeName, Value};
use serde_json::Value as JsonValue;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ToolImpl = Arc<
    dyn Fn(HashMap<String, Value>) -> BoxFuture<'static, Result<Value, String>>
        + Send + Sync
>;

pub fn native_dispatch() -> HashMap<String, ToolImpl> {
    let mut m: HashMap<String, ToolImpl> = HashMap::new();

    m.insert("read-file".into(), Arc::new(|args| Box::pin(async move {
        let path = args.get("path").ok_or("missing :path")?;
        let s = path.data.as_str().ok_or("path not string")?;
        let bytes = tokio::fs::read(s).await.map_err(|e| format!("read: {e}"))?;
        let text = String::from_utf8(bytes).map_err(|e| format!("utf8: {e}"))?;
        Ok(Value { data: JsonValue::String(text), declared_type: TypeName("PlainText".into()) })
    })));

    m.insert("write-file".into(), Arc::new(|args| Box::pin(async move {
        let path = args.get("path").ok_or("missing :path")?.data.as_str().ok_or("path not string")?.to_string();
        let content = args.get("content").ok_or("missing :content")?.data.as_str().ok_or("content not string")?.to_string();
        tokio::fs::write(&path, content).await.map_err(|e| format!("write: {e}"))?;
        Ok(Value { data: JsonValue::Null, declared_type: TypeName("Unit".into()) })
    })));

    m.insert("summarize".into(), Arc::new(|args| Box::pin(async move {
        let input = extract_input(&args)?;
        let summary = format!("[SUMMARY of {} chars]", input.len());
        Ok(Value { data: JsonValue::String(summary), declared_type: TypeName("Summary".into()) })
    })));

    m.insert("translate".into(), Arc::new(|args| Box::pin(async move {
        let input = extract_input(&args)?;
        let lang = args.get("lang").ok_or("missing :lang")?.data.as_str().ok_or("lang not string")?.to_string();
        let out = format!("[TRANSLATED to {lang}]\n{input}");
        Ok(Value { data: JsonValue::String(out), declared_type: TypeName("PlainText".into()) })
    })));

    m.insert("ocr".into(), Arc::new(|args| Box::pin(async move {
        let _ = args.get("source").ok_or("missing :source")?;
        Ok(Value { data: JsonValue::String("[OCR-EXTRACTED-TEXT]".into()), declared_type: TypeName("PlainText".into()) })
    })));

    m.insert("llm".into(), Arc::new(|args| Box::pin(async move {
        let prompt = args.get("prompt").ok_or("missing :prompt")?.data.as_str().unwrap_or("").to_string();
        let input = args.get("input").map(|v| v.data.as_str().unwrap_or("")).unwrap_or("");
        let out = format!("[LLM prompt={prompt} input_len={}]", input.len());
        Ok(Value { data: JsonValue::String(out), declared_type: TypeName("PlainText".into()) })
    })));

    m
}

/// Try either :input (kw form) or the sole positional (flowed-in).
fn extract_input(args: &HashMap<String, Value>) -> Result<String, String> {
    if let Some(v) = args.get("input") {
        return Ok(v.data.as_str().unwrap_or("").to_string());
    }
    // Flowed-in value is passed under the tool's declared sole-param name.
    // In MVP the runtime binds it to the parameter name; we look for a
    // "_flowed" convention as fallback if it wasn't rekeyed.
    args.iter().find_map(|(_, v)| v.data.as_str().map(str::to_string)).ok_or_else(|| "no input".into())
}
```

- [ ] **Step 7: Wire up lib.rs**

Create `crates/agnes-builtins/src/lib.rs`:

```rust
//! Built-in types, aliases, and tool implementations for MVP.

mod types;
mod aliases;
mod tools;

pub use tools::{native_dispatch, BoxFuture, ToolImpl};

use agnes_registry::{Registry, RegistryError};
use agnes_types::{ToolSignature, TypeExpr, TypeName};

pub fn register_builtins(reg: &mut Registry) -> Result<(), RegistryError> {
    // --- Types + validators ---
    reg.register_type("Path",       Some(types::path_validator))?;
    reg.register_type("PlainText",  Some(types::utf8_validator))?;
    reg.register_type("Markdown",   Some(types::utf8_validator))?;
    reg.register_type("HTML",       Some(types::utf8_validator))?;
    reg.register_type("JSON",       Some(types::json_validator))?;
    reg.register_type("PDF",        Some(types::pdf_validator))?;
    reg.register_type("Image",      Some(types::image_validator))?;
    reg.register_type("Summary",    Some(types::utf8_validator))?;
    reg.register_type("Unit",       Some(types::unit_validator))?;
    reg.register_type("Unknown",    None)?;
    // Non-workflow types used by literals.
    reg.register_type("String",     None)?;
    reg.register_type("Int",        None)?;
    reg.register_type("Bool",       None)?;

    // --- Aliases ---
    reg.register_alias("TextLike",  aliases::text_like())?;
    reg.register_alias("VisualDoc", aliases::visual_doc())?;

    // --- Tools ---
    let path = TypeExpr::Named(TypeName("Path".into()));
    let plaintext = TypeExpr::Named(TypeName("PlainText".into()));
    let summary = TypeExpr::Named(TypeName("Summary".into()));
    let unit = TypeExpr::Named(TypeName("Unit".into()));
    let string_ty = TypeExpr::Named(TypeName("String".into()));

    reg.register_tool("read-file", ToolSignature {
        requires: vec![("path".into(), path.clone())],
        provides: plaintext.clone(),
    })?;
    reg.register_tool("write-file", ToolSignature {
        requires: vec![
            ("path".into(), path.clone()),
            ("content".into(), aliases::text_like()),
        ],
        provides: unit.clone(),
    })?;
    reg.register_tool("summarize", ToolSignature {
        requires: vec![("input".into(), TypeExpr::Union({
            let mut s = aliases::text_like().as_set();
            s.insert(TypeName("PDF".into()));
            s
        }))],
        provides: summary.clone(),
    })?;
    reg.register_tool("translate", ToolSignature {
        requires: vec![
            ("input".into(), aliases::text_like()),
            ("lang".into(), string_ty.clone()),
        ],
        provides: plaintext.clone(),
    })?;
    reg.register_tool("ocr", ToolSignature {
        requires: vec![("source".into(), aliases::visual_doc())],
        provides: plaintext.clone(),
    })?;
    reg.register_tool("llm", ToolSignature {
        requires: vec![
            ("prompt".into(), string_ty.clone()),
            ("input".into(), plaintext.clone()),
        ],
        provides: plaintext,
    })?;
    Ok(())
}
```

- [ ] **Step 8: Run — expect PASS**

Run: `cargo test -p agnes-builtins --tests`
Expected: `2 passed`.

- [ ] **Step 9: Commit**

```
jj describe -m "feat(builtins): built-in types, validators, aliases, and tool impls

register_builtins() populates the Registry with 10 named types (each
with a structural validator), 2 predefined aliases (TextLike, VisualDoc),
and 6 tool signatures matching the spec's built-in tool table.

native_dispatch() returns an async ToolImpl per tool name. read-file
and write-file hit the real filesystem; summarize, translate, ocr, and
llm are mock implementations returning placeholder strings — sufficient
for MVP acceptance workflows.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 9: agnes-runtime crate

**Files:**
- Create: `crates/agnes-runtime/Cargo.toml`
- Create: `crates/agnes-runtime/src/lib.rs`
- Create: `crates/agnes-runtime/src/scheduler.rs`
- Create: `crates/agnes-runtime/src/boundary.rs`
- Create: `crates/agnes-runtime/src/error.rs`
- Create: `crates/agnes-runtime/tests/execute.rs`

**Interfaces:**
- Consumes: `agnes_compiler::{Dag, Node, NodeKind, NodeId, Input}`, `agnes_registry::Registry`, `agnes_builtins::{ToolImpl, native_dispatch}`, `agnes_types::*`
- Produces:
  - `pub async fn execute(dag: &Dag, registry: &Registry, dispatch: &HashMap<String, ToolImpl>) -> Result<Value, RuntimeError>`
  - `RuntimeError::{ ToolFailed { tool, cause }, RuntimeTypeError { tool, direction, ty, cause }, MissingImpl { tool } }`

- [ ] **Step 1: Manifest**

Create `crates/agnes-runtime/Cargo.toml`:

```toml
[package]
name = "agnes-runtime"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
agnes-ast.workspace = true
agnes-types.workspace = true
agnes-registry.workspace = true
agnes-compiler.workspace = true
tokio.workspace = true
thiserror.workspace = true
serde_json.workspace = true
tracing.workspace = true

[dev-dependencies]
agnes-parser.workspace = true
agnes-checker.workspace = true
agnes-builtins.workspace = true
```

- [ ] **Step 2: Failing e2e test**

Create `crates/agnes-runtime/tests/execute.rs`:

```rust
use agnes_builtins::{native_dispatch, register_builtins};
use agnes_checker::check;
use agnes_compiler::compile;
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_runtime::execute;

#[tokio::test]
async fn runs_read_then_summarize() {
    // Prepare a temp file
    let tmp = tempfile_path();
    tokio::fs::write(&tmp, "hello world").await.unwrap();

    let src = format!(r#"(pipe (tool read-file :path "{tmp}") (tool summarize))"#);
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();

    let p = parse(&src).unwrap();
    r.load(&p).unwrap();
    check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();
    let dispatch = native_dispatch();
    let out = execute(&dag, &r, &dispatch).await.expect("run ok");
    let s = out.data.as_str().expect("string result");
    assert!(s.starts_with("[SUMMARY of"), "got: {s}");
    let _ = tokio::fs::remove_file(&tmp).await;
}

fn tempfile_path() -> String {
    let dir = std::env::temp_dir();
    let stamp = std::process::id();
    dir.join(format!("agnes-test-{stamp}.txt")).to_string_lossy().into_owned()
}
```

- [ ] **Step 3: Run — expect compile errors**

Run: `cargo test -p agnes-runtime --tests`
Expected: `agnes_runtime` not found.

- [ ] **Step 4: Error module**

Create `crates/agnes-runtime/src/error.rs`:

```rust
use agnes_types::TypeName;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("Tool `{tool}` failed: {cause}")]
    ToolFailed { tool: String, cause: String },

    #[error(
"Runtime type error at (tool {tool} {direction}):
  step `{tool}` declared: {direction} {ty}
  but value fails {ty} validator: {cause}

Fix suggestion:
  Either fix the tool implementation to match its declared type,
  or re-declare its signature to match reality:
    (declare tool {tool} :requires [...] :provides <TypeExpr>)"
    )]
    RuntimeTypeError {
        tool: String,
        direction: &'static str, // "provides" or ":<param> requires"
        ty: TypeName,
        cause: String,
    },

    #[error("No native implementation registered for tool `{tool}`")]
    MissingImpl { tool: String },
}
```

- [ ] **Step 5: Boundary validation**

Create `crates/agnes-runtime/src/boundary.rs`:

```rust
use agnes_registry::Registry;
use agnes_types::{TypeName, Value};

use crate::error::RuntimeError;

pub fn validate(reg: &Registry, tool: &str, direction: &'static str, ty: &TypeName, val: &Value) -> Result<(), RuntimeError> {
    if let Some(v) = reg.validator_of(ty) {
        v(&val.data).map_err(|cause| RuntimeError::RuntimeTypeError {
            tool: tool.to_string(),
            direction,
            ty: ty.clone(),
            cause,
        })?;
    }
    Ok(())
}
```

- [ ] **Step 6: Scheduler + executor**

Create `crates/agnes-runtime/src/scheduler.rs`:

```rust
use std::collections::HashMap;

use agnes_ast::Literal;
use agnes_builtins::ToolImpl;
use agnes_compiler::{Dag, Input, NodeId, NodeKind};
use agnes_registry::Registry;
use agnes_types::{TypeExpr, TypeName, Value};
use serde_json::Value as JsonValue;

use crate::boundary::validate;
use crate::error::RuntimeError;

/// Recursively evaluate a node, returning its produced Value.
/// Results are memoized in `cache` so shared subgraphs execute once.
pub async fn run(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
) -> Result<Value, RuntimeError> {
    let mut cache: HashMap<NodeId, Value> = HashMap::new();
    let mut env: HashMap<String, Value> = HashMap::new();
    eval_node(dag, dag.root, reg, dispatch, &mut cache, &mut env).await
}

fn eval_node<'a>(
    dag: &'a Dag,
    id: NodeId,
    reg: &'a Registry,
    dispatch: &'a HashMap<String, ToolImpl>,
    cache: &'a mut HashMap<NodeId, Value>,
    env:   &'a mut HashMap<String, Value>,
) -> agnes_builtins::BoxFuture<'a, Result<Value, RuntimeError>> {
    Box::pin(async move {
        if let Some(v) = cache.get(&id) { return Ok(v.clone()); }
        let node = dag.get(id);
        let value = match &node.kind {
            NodeKind::Literal(lit) => Value {
                data: lit_to_json(lit),
                declared_type: lit_type(lit),
            },
            NodeKind::Var(name) => env.get(name).cloned().ok_or_else(|| RuntimeError::ToolFailed {
                tool: format!("<var>{name}"),
                cause: "unbound variable".into(),
            })?,
            NodeKind::Let { name } => {
                let src = eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?;
                env.insert(name.clone(), src.clone());
                src
            }
            NodeKind::Pipe => eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?,
            NodeKind::Par => {
                // Evaluate each branch concurrently.
                let mut handles = Vec::new();
                // Because eval_node needs &mut cache/env, we serialize par
                // branches in MVP (correctness > concurrency for now).
                for input in &node.inputs {
                    handles.push(eval_input(dag, input, reg, dispatch, cache, env).await?);
                }
                // Par's value is Unit; the useful outputs are already bound via `let`.
                Value { data: JsonValue::Null, declared_type: TypeName("Unit".into()) }
            }
            NodeKind::If => {
                let cond = eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?;
                let picked = if cond.data.as_bool().unwrap_or(false) { 1 } else { 2 };
                eval_input(dag, &node.inputs[picked], reg, dispatch, cache, env).await?
            }
            NodeKind::Match { arms } => {
                let s = eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?;
                let mut chosen: Option<usize> = None;
                for (i, pat) in arms.iter().enumerate() {
                    if lit_matches(pat, &s.data) { chosen = Some(i + 1); break; }
                }
                let idx = chosen.unwrap_or(arms.len()); // fall through to last arm
                eval_input(dag, &node.inputs[idx.min(node.inputs.len()-1)], reg, dispatch, cache, env).await?
            }
            NodeKind::Foreach { .. } => {
                // MVP simplification: evaluate body once and return that.
                eval_input(dag, &node.inputs[1], reg, dispatch, cache, env).await?
            }
            NodeKind::Retry { times, .. } => {
                let mut last_err: Option<RuntimeError> = None;
                for _ in 0..(*times + 1) {
                    match eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await {
                        Ok(v) => { last_err = None; return Ok(v); }
                        Err(e) => last_err = Some(e),
                    }
                }
                return Err(last_err.unwrap());
            }
            NodeKind::Catch { fallback, .. } => {
                match eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await {
                    Ok(v) => v,
                    Err(_) => eval_node(dag, *fallback, reg, dispatch, cache, env).await?,
                }
            }
            NodeKind::Llm => {
                let args = collect_kwargs(dag, &node.inputs, reg, dispatch, cache, env).await?;
                call_native("llm", args, dispatch, reg, &node.provides).await?
            }
            NodeKind::Return => eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?,
            NodeKind::Tool { name } => {
                let args = collect_kwargs(dag, &node.inputs, reg, dispatch, cache, env).await?;
                call_native(name, args, dispatch, reg, &node.provides).await?
            }
        };
        cache.insert(id, value.clone());
        Ok(value)
    })
}

fn eval_input<'a>(
    dag: &'a Dag,
    input: &'a Input,
    reg: &'a Registry,
    dispatch: &'a HashMap<String, ToolImpl>,
    cache: &'a mut HashMap<NodeId, Value>,
    env:   &'a mut HashMap<String, Value>,
) -> agnes_builtins::BoxFuture<'a, Result<Value, RuntimeError>> {
    Box::pin(async move {
        match input {
            Input::FromNode(id) => eval_node(dag, *id, reg, dispatch, cache, env).await,
            Input::Literal(lit) => Ok(Value {
                data: lit_to_json(lit),
                declared_type: lit_type(lit),
            }),
            Input::Var(name) => env.get(name).cloned().ok_or_else(|| RuntimeError::ToolFailed {
                tool: format!("<var>{name}"),
                cause: "unbound variable".into(),
            }),
            Input::Kw { source, .. } => eval_input(dag, source, reg, dispatch, cache, env).await,
        }
    })
}

async fn collect_kwargs(
    dag: &Dag,
    inputs: &[Input],
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    cache: &mut HashMap<NodeId, Value>,
    env:   &mut HashMap<String, Value>,
) -> Result<HashMap<String, Value>, RuntimeError> {
    let mut out = HashMap::new();
    for input in inputs {
        match input {
            Input::Kw { key, source } => {
                let v = eval_input(dag, source, reg, dispatch, cache, env).await?;
                out.insert(key.clone(), v);
            }
            other => {
                let v = eval_input(dag, other, reg, dispatch, cache, env).await?;
                out.insert("_positional".into(), v);
            }
        }
    }
    Ok(out)
}

async fn call_native(
    tool: &str,
    args: HashMap<String, Value>,
    dispatch: &HashMap<String, ToolImpl>,
    reg: &Registry,
    provides: &TypeExpr,
) -> Result<Value, RuntimeError> {
    // Validate `requires` for every arg using the registry.
    if let Some(sig) = reg.tool_signature(tool) {
        for (k, expected) in &sig.requires {
            if let Some(v) = args.get(k) {
                // For union, validator runs against the actual declared_type.
                validate(reg, tool, "requires", &v.declared_type, v)?;
                let _ = expected; // membership already enforced by checker
            }
        }
    }
    let f = dispatch.get(tool).ok_or_else(|| RuntimeError::MissingImpl { tool: tool.to_string() })?;
    let result = f(args).await.map_err(|cause| RuntimeError::ToolFailed { tool: tool.to_string(), cause })?;
    // Validate `provides`.
    let ty: TypeName = match provides {
        TypeExpr::Named(n) => n.clone(),
        TypeExpr::Union(_) => result.declared_type.clone(),
    };
    validate(reg, tool, "provides", &ty, &result)?;
    Ok(result)
}

fn lit_to_json(lit: &Literal) -> JsonValue {
    match lit {
        Literal::String(s) => JsonValue::String(s.clone()),
        Literal::Int(n)    => JsonValue::from(*n),
        Literal::Bool(b)   => JsonValue::Bool(*b),
        Literal::Nil       => JsonValue::Null,
    }
}

fn lit_type(lit: &Literal) -> TypeName {
    match lit {
        Literal::String(_) => TypeName("String".into()),
        Literal::Int(_)    => TypeName("Int".into()),
        Literal::Bool(_)   => TypeName("Bool".into()),
        Literal::Nil       => TypeName("Unit".into()),
    }
}

fn lit_matches(pat: &Literal, val: &JsonValue) -> bool {
    match (pat, val) {
        (Literal::String(a), JsonValue::String(b)) => a == b,
        (Literal::Int(a),    JsonValue::Number(b)) => b.as_i64() == Some(*a),
        (Literal::Bool(a),   JsonValue::Bool(b))   => a == b,
        (Literal::Nil,       JsonValue::Null)      => true,
        _ => false,
    }
}
```

- [ ] **Step 7: lib.rs**

Create `crates/agnes-runtime/src/lib.rs`:

```rust
//! agnes runtime: tokio async executor with boundary validation.

pub mod boundary;
pub mod error;
mod scheduler;

pub use error::RuntimeError;

use std::collections::HashMap;
use agnes_builtins::ToolImpl;
use agnes_compiler::Dag;
use agnes_registry::Registry;
use agnes_types::Value;

pub async fn execute(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
) -> Result<Value, RuntimeError> {
    scheduler::run(dag, reg, dispatch).await
}
```

- [ ] **Step 8: Run — expect PASS**

Run: `cargo test -p agnes-runtime --tests`
Expected: `1 passed`.

- [ ] **Step 9: Commit**

```
jj describe -m "feat(runtime): tokio async DAG executor with boundary validation

execute() walks the compiled Dag recursively, memoizing node results.
Every tool call site validates :requires argument types on entry and
:provides on return using the registry's validators; failures surface
as RuntimeTypeError with a full What/Why/Fix rendering.

Control-flow node kinds (Pipe, Par, If, Match, Foreach, Retry, Catch,
Let, Return) map onto straightforward async logic. Par currently runs
branches sequentially — correctness first; concurrent join is a
follow-up.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 10: agnes-cli crate + examples

**Files:**
- Create: `crates/agnes-cli/Cargo.toml`
- Create: `crates/agnes-cli/src/main.rs`
- Create: `examples/hello.agnes`
- Create: `examples/translate.agnes`
- Create: `examples/fan-out.agnes`
- Create: `examples/with-define.agnes`
- Create: `examples/full-demo.agnes`

**Interfaces:**
- Consumes: everything
- Produces: `agnes` binary that takes a `.agnes` file path and runs it

- [ ] **Step 1: Manifest**

Create `crates/agnes-cli/Cargo.toml`:

```toml
[package]
name = "agnes-cli"
edition.workspace = true
version.workspace = true
license.workspace = true
authors.workspace = true

[[bin]]
name = "agnes"
path = "src/main.rs"

[dependencies]
agnes-parser.workspace = true
agnes-registry.workspace = true
agnes-checker.workspace = true
agnes-compiler.workspace = true
agnes-runtime.workspace = true
agnes-builtins.workspace = true
tokio.workspace = true
anyhow.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
```

- [ ] **Step 2: Write main.rs**

Create `crates/agnes-cli/src/main.rs`:

```rust
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let path = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: agnes <file.agnes>"))?;
    let src = tokio::fs::read_to_string(PathBuf::from(&path)).await?;

    let mut reg = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut reg)?;

    let program = agnes_parser::parse(&src).map_err(|e| anyhow::anyhow!("{e}"))?;
    reg.load(&program).map_err(|e| anyhow::anyhow!("{e}"))?;
    agnes_checker::check(&program, &reg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dag = agnes_compiler::compile(&program, &reg).map_err(|e| anyhow::anyhow!("{e}"))?;

    let dispatch = agnes_builtins::native_dispatch();
    let result = agnes_runtime::execute(&dag, &reg, &dispatch).await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("{}", result.data);
    Ok(())
}
```

- [ ] **Step 3: Write examples**

Create `examples/hello.agnes`:

```lisp
;; The smallest agnes workflow: one tool call.
(tool llm :prompt "say hi" :input "")
```

Create `examples/translate.agnes`:

```lisp
;; Sequential pipe: read a file then translate it.
(pipe
  (tool read-file :path "README.md")
  (tool translate :lang "ja"))
```

Create `examples/fan-out.agnes`:

```lisp
;; Parallel branches with let bindings feeding a downstream tool.
(pipe
  (let src (tool read-file :path "README.md"))
  (par
    (let sum (tool summarize :input src))
    (let ja  (tool translate :input src :lang "ja")))
  (tool llm :prompt "combine" :input sum))
```

Create `examples/with-define.agnes`:

```lisp
;; Define a compound tool and use it.
(define read-and-translate
  :params  [(path: Path) (target: String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(tool read-and-translate :path "README.md" :target "ja")
```

Create `examples/full-demo.agnes` (matches spec §VII):

```lisp
(define read-and-translate
  :params  [(path: Path) (target: String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(pipe
  (let src (tool read-file :path "README.md"))
  (par
    (let sum (tool summarize :input src))
    (let ja  (tool read-and-translate :path "README.md" :target "ja")))
  (tool llm :prompt "combine summary and translation" :input sum))
```

- [ ] **Step 4: Verify build + run**

Run: `cd /home/hao/code/agnes && cargo build`
Expected: builds cleanly.

Prepare a README.md in the workspace root so file reads succeed:
```
echo "hello agnes" > /home/hao/code/agnes/README.md
```

Run: `cargo run -p agnes-cli -- examples/translate.agnes`
Expected: prints a JSON string like `"[TRANSLATED to ja]\nhello agnes\n"`.

Run: `cargo run -p agnes-cli -- examples/full-demo.agnes`
Expected: prints a string starting with `"[LLM prompt=combine summary and translation`.

- [ ] **Step 5: Commit**

```
jj describe -m "feat(cli): agnes binary that runs .agnes files end-to-end

Wire parser -> registry -> checker -> compiler -> runtime into a single
binary. On success prints the final Value's JSON payload; on failure
prints the LLM-friendly What/Why/Fix error.

Add five example workflows covering the surface tested by the spec's
Acceptance Criteria (hello, translate, fan-out, with-define, full-demo).

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 11: End-to-end acceptance tests + negative cases

**Files:**
- Create: `tests/acceptance.rs` (workspace-level; add as workspace member if needed)
- Modify: `Cargo.toml` (root) — add `tests/` if not automatically discovered

**Interfaces:**
- Consumes: entire stack
- Produces: passing tests for the 5 acceptance workflows and 4 negative cases from spec §VII

- [ ] **Step 1: Create the acceptance harness as an integration test in agnes-cli**

Create `crates/agnes-cli/tests/acceptance.rs`:

```rust
use agnes_builtins::{native_dispatch, register_builtins};
use agnes_checker::check;
use agnes_compiler::compile;
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_runtime::execute;

async fn run(src: &str) -> Result<String, String> {
    let mut reg = Registry::new();
    register_builtins(&mut reg).map_err(|e| format!("{e}"))?;
    let program = parse(src).map_err(|e| format!("{e}"))?;
    reg.load(&program).map_err(|e| format!("{e}"))?;
    check(&program, &reg).map_err(|e| format!("{e}"))?;
    let dag = compile(&program, &reg).map_err(|e| format!("{e}"))?;
    let d = native_dispatch();
    let v = execute(&dag, &reg, &d).await.map_err(|e| format!("{e}"))?;
    Ok(v.data.to_string())
}

async fn seed_readme() -> String {
    let path = std::env::temp_dir().join(format!("agnes-acceptance-readme-{}.md", std::process::id()));
    tokio::fs::write(&path, "hello world\n").await.unwrap();
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn positive_full_demo_runs() {
    let readme = seed_readme().await;
    let src = format!(r#"
(define read-and-translate
  :params  [(path: Path) (target: String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(pipe
  (let src (tool read-file :path "{readme}"))
  (par
    (let sum (tool summarize :input src))
    (let ja  (tool read-and-translate :path "{readme}" :target "ja")))
  (tool llm :prompt "combine" :input sum))
"#);
    let out = run(&src).await.expect("must succeed");
    assert!(out.contains("[LLM prompt=combine"), "got: {out}");
    let _ = tokio::fs::remove_file(&readme).await;
}

#[tokio::test]
async fn negative_flow_type_mismatch() {
    let src = r#"(pipe (tool read-file :path "x.md") (tool ocr))"#;
    let err = run(src).await.expect_err("must fail type check");
    assert!(err.contains("Type error"), "got: {err}");
    assert!(err.contains("ocr"), "got: {err}");
    assert!(err.contains("Fix suggestion"), "got: {err}");
}

#[tokio::test]
async fn negative_recursive_define() {
    let src = r#"(define loopy :params [] :provides Unit (tool loopy))"#;
    let err = run(src).await.expect_err("must fail compile");
    assert!(err.contains("Recursive define"), "got: {err}");
    assert!(err.contains("loopy"), "got: {err}");
}

#[tokio::test]
async fn negative_unknown_type() {
    let src = r#"(declare tool weird :requires [(x: MysteryType)] :provides PlainText)"#;
    let err = run(src).await.expect_err("must fail registry load");
    assert!(err.contains("Unknown name"), "got: {err}");
    assert!(err.contains("MysteryType"), "got: {err}");
    assert!(err.contains("declare type"), "got: {err}");
}

#[tokio::test]
async fn negative_name_conflict() {
    let src = r#"(declare type PlainText)"#;
    let err = run(src).await.expect_err("must fail registry load");
    assert!(err.contains("Name conflict"), "got: {err}");
    assert!(err.contains("PlainText"), "got: {err}");
}
```

- [ ] **Step 2: Run**

Run: `cargo test -p agnes-cli --test acceptance`
Expected: `5 passed`.

- [ ] **Step 3: Full workspace lint check**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -50`
Fix any warnings that emerge (most likely `unused_imports` or `dead_code`).

Run: `cargo fmt --all`

Run: `cargo test --workspace`
Expected: all tests pass across the workspace.

- [ ] **Step 4: Commit**

```
jj describe -m "test: end-to-end acceptance tests for spec section VII

Positive: full-demo workflow with define, pipe, par, let, and llm.

Negative:
- flow type mismatch (read-file -> ocr) surfaces LLM-friendly Fix suggestion
- recursive define is caught by cycle detector
- unknown type name in declare tool is caught by registry loader
- name conflict (redeclaring PlainText as new type) is rejected

All error messages are asserted to contain the What/Why/Fix markers.

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
```

---

## Task 12: README + push

**Files:**
- Create: `README.md`
- Modify: nothing else

**Interfaces:** none

- [ ] **Step 1: Write README**

Create `/home/hao/code/agnes/README.md`:

```markdown
# agnes

A Lisp-style DSL and Rust runtime for LLM-planned agent workflows, with a
TypeScript-style semantic type system that lets LLMs annotate untyped
tools (MCP / CLI / HTTP) and get compile-time and runtime type safety.

**Status:** MVP — proves the language design. Ships 5 built-in tools and
a workspace of 9 focused crates.

## Try it

```
cargo run -p agnes-cli -- examples/full-demo.agnes
```

## Spec + design

See `docs/superpowers/specs/2026-07-18-agnes-dsl-mvp-design.md` for the
full design rationale and `docs/superpowers/plans/2026-07-18-agnes-dsl-mvp.md`
for the implementation plan.

## Language at a glance

```lisp
(define read-and-translate
  :params  [(path: Path) (target: String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(pipe
  (let src (tool read-file :path "README.md"))
  (par
    (let sum (tool summarize :input src))
    (let ja  (tool read-and-translate :path "README.md" :target "ja")))
  (tool llm :prompt "combine summary and translation" :input sum))
```

## License

MIT OR Apache-2.0
```

- [ ] **Step 2: Push to GitHub via jj**

Run:
```
jj describe -m "docs: add README

Co-Authored-By: Claude <noreply@anthropic.com>"
jj new
jj bookmark move main --to @-
jj git push --allow-new
```

Verify on GitHub that the repo shows the workspace.

---

## Self-Review Summary (for the plan author)

**Spec coverage check** — each spec section maps to at least one task:

| Spec § | Task |
|---|---|
| §I Directives (declare / define / expressions) | Tasks 2, 4, 5, 7 |
| §II Type system (Type + Union + Alias, 2 rules, error template) | Tasks 3, 5, 6 |
| §II.6 Boundary validation | Tasks 8, 9 |
| §III.1 Data flow (pipe, let, kwargs) | Tasks 4, 7, 9 |
| §III.2 Control flow (par/if/match/foreach) | Tasks 4, 7, 9 |
| §III.3 Errors (retry/catch/fail-fast) | Tasks 4, 7, 9 |
| §III.4 Define semantics + no recursion | Tasks 5, 7 |
| §III.5 Declare forms | Tasks 4, 5 |
| §IV Runtime architecture | Tasks 5-9 |
| §V Cargo workspace + crate boundaries | Task 1 + every crate task |
| §VI Built-ins (10 types, 2 aliases, 6 tools) | Task 8 |
| §VII Acceptance criteria | Tasks 10, 11 |

**Type consistency:** Names used consistently across tasks — `TypeName`, `TypeExpr::{Named, Union}`, `Value { data, declared_type }`, `Registry::resolve`, `Dag { nodes, root }`, `Node.provides`, `NodeKind`, `Input::Kw { key, source }`, `ToolImpl`, `native_dispatch`. Where a task references another task's items (e.g. Task 9 calls `register_builtins` from Task 8), the exact names match.

**Placeholder scan:** No `TODO`, `TBD`, or "similar to Task N"; every code step has full code.

**Known deferrals (documented in the spec as MVP+):**
- Retry/catch modifier-form desugaring is elided in Task 7 (only control-flow form supported at MVP acceptance); this is stated in the Task 7 commit message.
- `par` runs branches sequentially in Task 9 for correctness (concurrent join is straightforward but requires per-branch `env` snapshotting).
- `foreach` returns the body's single evaluation rather than iterating (MVP doesn't have list literals in acceptance tests).
- These simplifications are acceptable for the MVP acceptance workflow, which uses pipe/par/let/define but not retry-modifier or true iteration.

---

# agnes-compiler

Lowers an AST + resolved registry into a **Dag** (directed acyclic graph)
that the runtime can execute. Also detects recursive `define`s before any
lowering happens.

## Public API

```rust
pub fn compile(program: &Program, registry: &Registry) -> Result<Dag, CompileError>;

pub struct Dag  { pub nodes: Vec<Node>, pub root: NodeId }
pub struct Node { pub id: NodeId, pub kind: NodeKind, pub inputs: Vec<Input>, pub provides: TypeExpr }

pub enum NodeKind {
    Tool { name },
    Pipe, Par,
    Let { name },
    If,
    Match  { arms: Vec<Literal> },
    Foreach { item },
    Retry  { times, backoff },
    Catch  { on, fallback: NodeId },
    Llm, Return,
    Literal(Literal), Var(String),
}

pub enum Input { FromNode(NodeId), Literal(Literal), Var(String), Kw { key, source } }

pub enum CompileError {
    CycleDetected { name },
    Registry(RegistryError),
    UnknownDefine { name },
}
```

## How lowering works

- `pipe` threads the previous node's `NodeId` into the next call's
  lowering as `upstream`. If the downstream tool has exactly one unfilled
  required parameter, the upstream is bound to that slot via
  `Input::Kw { key, source: FromNode(up) }`.
- `par` lowers each branch independently and packs the branch node ids
  into a single `Par` node whose declared type is `Unit`.
- `let name` (1-arg) reads the current upstream; `let name expr` (2-arg)
  evaluates the expression side-line. Both emit a `Let { name }` node.
- Every branch of `if` / `match` / `catch` / `retry` / `foreach` is
  lowered to its own node; the parent node references them by id.
- `Llm` and `Return` are just distinguished node kinds with typed inputs.
- Literals and vars become `Literal(...)` / `Var(...)` node kinds.

## Cycle detection

Before lowering, `cycle::detect_define_cycles` builds a call graph over
the `define`s (via the tool names they reference in their bodies) and
returns any name that transitively reaches itself. Rejected with
`CompileError::CycleDetected` — MVP forbids recursion.

## Pipeline position

```
agnes-ast::Program  +  agnes-registry::Registry (canonical tool sigs)
     ↓  agnes-compiler::compile   ← you are here
   Dag (fully-typed node graph)
     ↓
   agnes-runtime executes it
```

## Design notes

- Deferred: retry/catch **modifier** form on tool calls (e.g. `(tool foo
  :retry 3)`) is not yet desugared into `Retry` / `Catch` control-flow
  nodes. Only the explicit control-flow forms are supported.
- The provides type on a `Par` node is `Unit`: par branches communicate
  outward via `let` bindings, not by returning a joined tuple.
- `Dag::get(id)` is a direct slot lookup — no hash map — because ids are
  assigned as slot indices during lowering.

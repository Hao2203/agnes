# agnes-runtime

Async DAG executor for compiled agnes programs. Walks the graph
recursively, memoizes shared subgraphs, dispatches to native tool
implementations, and validates every tool boundary against the registry's
validators.

## Public API

```rust
pub async fn execute(
    dag: &Dag,
    registry: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
) -> Result<Value, RuntimeError>;

pub enum RuntimeError {
    ToolFailed       { tool, cause },
    RuntimeTypeError { tool, direction, ty, cause },
    MissingImpl      { tool },
}
```

`RuntimeTypeError` renders as a full **What / Why / Fix suggestion**
error identifying which side (`requires` or `provides`) failed which
validator.

## Boundary validation

At every tool call site:

1. For each provided arg, look up its declared type's validator via
   `Registry::validator_of` and run it. Failure → `RuntimeTypeError` with
   `direction = "requires"`.
2. Call the native implementation.
3. Run the provides validator on the returned `Value`. Failure →
   `RuntimeTypeError` with `direction = "provides"`.

Types with no validator (e.g. `String`, `Int`, `Bool`, `Unknown`) skip
validation but still participate in the checker's set-membership rules.

## Node kind → behaviour

| NodeKind      | What the runtime does                                                     |
|---------------|---------------------------------------------------------------------------|
| `Tool { name }` | Collect kwargs, validate requires, dispatch, validate provides          |
| `Llm`         | Same as `Tool`, fixed to `llm` dispatch                                   |
| `Pipe`        | Evaluate the tail node (all threading was resolved at compile time)       |
| `Par`         | Evaluate each branch (MVP runs sequentially; returns `Unit`)              |
| `Let { name }`| Evaluate source, bind result into scoped `env`, return the value          |
| `If`          | Evaluate cond, then branch or else branch                                 |
| `Match`       | Evaluate scrutinee, choose the first arm whose literal matches            |
| `Foreach`     | MVP simplification: evaluate the body once (no list literals in acceptance) |
| `Retry`       | Try body up to `times + 1` attempts; return last error if all fail        |
| `Catch`       | Try body; on any error, evaluate the fallback node                        |
| `Return`      | Passthrough                                                               |
| `Literal(l)`  | Materialize `Value { data: json_of(l), declared_type: type_of(l) }`       |
| `Var(name)`   | Look up in the runtime `env`                                              |

## Pipeline position

```
Dag  +  Registry  +  dispatch
     ↓  agnes-runtime::execute   ← you are here
Value or RuntimeError
```

## Design notes

- Results are memoized by `NodeId`, so multiple `let`-bound consumers of
  the same producer only run it once.
- `Par` currently runs branches sequentially so branches can share the
  mutable `env` / `cache` maps. A concurrent join is straightforward but
  requires per-branch env snapshotting or interior mutability.
- The scheduler returns `BoxFuture` from a recursive async fn — Rust
  requires the boxing because the future references its own type.
- User-defined `define` bodies are dispatched via a compound tool wrapper
  the compiler installs; there's no distinction at the executor level.

## Tests

`tests/execute.rs` covers:

- `read-file → summarize` end-to-end with a temp file
- calling a user-defined compound tool

Run: `cargo test -p agnes-runtime`.

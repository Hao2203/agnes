# agnes-checker

Static type checker for the agnes DSL. Enforces exactly the three rules
from the spec, and nothing else:

1. **Parameter satisfaction** — every keyword arg's type satisfies the
   corresponding require's `TypeExpr`.
2. **Flow satisfaction** — in a `pipe`, if the downstream tool has
   exactly one unfilled required parameter, the upstream's provides
   must satisfy that parameter's `TypeExpr`.
3. **Empty-list literal adaptation** — an empty list literal `[]` in a
   context that expects `(List T)` adopts `T` from the hint; otherwise
   it defaults to `(List Unknown)`.

Rules 1 and 2 bottom out at `agnes_types::type_expr_matches` (recursive
structural match with union membership at every `|` node).

## Public API

```rust
pub fn check(program: &Program, registry: &Registry) -> Result<(), CheckError>;

pub enum CheckError {
    ParamMismatch { tool, param, expected, actual },
    FlowMismatch  { upstream, downstream_tool, expected, actual },
    UnknownTool   { name },
    UnknownVar    { name },
    DefineSignatureMismatch { name, declared, body_type },
    Registry(RegistryError),
}
```

Every variant renders as a **What / Why / Fix suggestion** three-section
message per the spec template. Error text is snapshot-tested (see
`tests/snapshots/`) so it doesn't drift accidentally.

## Walk order

`check(program, registry)`:

1. For each `define` top-level: seed an `Env` with its params (as their
   resolved single-member types), walk the body, verify the body's type
   satisfies the declared `:provides`.
2. If a main expression exists, walk it with an empty `Env`.

`check_expr(e, reg, env, flowed_in, hint)` returns the `TypeExpr` the
expression produces. `flowed_in` is `Some(...)` only inside a `pipe` where
the current step is not the first, letting the flow rule fire. `hint` is
`Some(&expected)` only when checking a tool arg (or the like), so that a
bare `[]` literal can adopt the container element type from context per
rule 3.

## Pipeline position

```
agnes-ast::Program  +  agnes-registry::Registry
     ↓  agnes-checker::check    ← you are here
     (either Ok(()) or an LLM-friendly CheckError)
     ↓
   agnes-compiler
```

## Design notes

- The checker is intentionally minimal: two rules, no subtyping,
  no inference beyond "what does this expression produce".
- Union `provides` on a tool is rejected in MVP because the flow rule
  needs a concrete producer type. Extend `check_tool_call` if the language
  grows to support downstream disjunction.
- The `env` module only exposes `get` / `set` — bindings are per-scope and
  the caller controls scoping.

## Tests

`tests/check.rs` covers the happy path, the flow-mismatch error rendering
(via `insta` snapshot), and unknown-tool errors. Run:

```
cargo test -p agnes-checker
```

If you change an error message, run `cargo insta review` to accept.

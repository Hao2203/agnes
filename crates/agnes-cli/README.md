# agnes-cli

Command-line entry point. Wires **parser → registry → checker → compiler
→ runtime** into a single `agnes` binary that runs a `.agnes` file.

## Usage

```
agnes <path/to/file.agnes>
```

On success, prints the final `Value`'s JSON payload to stdout. On failure,
prints the LLM-friendly What / Why / Fix suggestion error and exits with a
non-zero status.

## Run without installing

From the workspace root:

```
cargo run -p agnes-cli -- examples/full-demo.agnes
```

## Install

```
cargo install --path crates/agnes-cli
agnes examples/full-demo.agnes
```

## What it does, step by step

```rust
let src = tokio::fs::read_to_string(path).await?;

let mut reg = Registry::new();
agnes_builtins::register_builtins(&mut reg)?;  // 13 types + 2 aliases + 7 tools

let program = agnes_parser::parse(&src)?;
reg.load(&program)?;                            // user declares merge in
agnes_checker::check(&program, &reg)?;          // three spec rules (param, flow, empty-list adapt)
let dag = agnes_compiler::compile(&program, &reg)?;

let dispatch = agnes_builtins::native_dispatch();
let result = agnes_runtime::execute(&dag, &reg, &dispatch).await?;

println!("{}", result.data);
```

## Logging

Uses `tracing_subscriber` with `EnvFilter::from_default_env`. Set
`RUST_LOG` to control verbosity:

```
RUST_LOG=debug cargo run -p agnes-cli -- examples/full-demo.agnes
```

## Examples

Ship in `examples/` at the workspace root:

- `hello.agnes` — single tool call
- `translate.agnes` — sequential pipe
- `fan-out.agnes` — par + let
- `with-define.agnes` — compound tool via `define`
- `full-demo.agnes` — the spec's acceptance workflow

## Acceptance tests

`tests/acceptance.rs` runs three positive workflows (the full-demo
`define + pipe + par + let + join-lines` shape, `join-lines` over a
list literal of tool calls, and a compound tool with an
`(Option String)` param) plus ten negative cases: `(List ...)` /
`(Option ...)` arity mismatches, unknown constructor head, infix-union
rejection, comma-in-list rejection, mixed-element-type list, flow
mismatch, recursive define, unknown type name, and name conflict.
Every error message is asserted to carry the
`What / Why / Fix suggestion` markers.

Run: `cargo test -p agnes-cli --test acceptance`.

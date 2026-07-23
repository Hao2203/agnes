# agnes-builtins

MVP built-ins: types (with validators) and native tool implementations. This is
the crate that turns "the language exists" into "you can run something useful".

## Public API

```rust
pub fn register_builtins(reg: &mut Registry) -> Result<(), RegistryError>;
pub fn native_dispatch() -> HashMap<String, ToolImpl>;

pub type ToolImpl = Arc<
    dyn Fn(HashMap<String, Value>) -> BoxFuture<'static, Result<Value, String>>
    + Send + Sync
>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
```

## What `register_builtins` installs

**Types + validators (10):**

| Type             | Validator                                    |
|------------------|----------------------------------------------|
| `Path`           | non-empty JSON string, no NUL byte           |
| `JSON`           | UTF-8 string parseable as JSON               |
| `Unit`           | JSON `null` or `{}`                          |
| `Unknown`        | no validator                                 |
| `String`         | (no validator; used for text literals)       |
| `Int`, `Bool`    | (no validators; used for literals)           |
| `CommandResult`  | (no validator; shell-run output)             |
| `Finish`         | (no validator; wrapper for the `finish` form)|
| `Observation`    | (no validator; wrapper for the `observe` form)|

`Finish` and `Observation` are wrapper types recognised by `show_value` /
`classify_root`; they wrap values produced by the `finish` and `observe`
special forms, which are not themselves tools.

**Tools (8):**

| Tool         | Requires                          | Provides        |
|--------------|-----------------------------------|-----------------|
| `read-file`  | `(path Path)`                     | `String`        |
| `write-file` | `(path Path)`, `(content String)` | `Unit`          |
| `summarize`  | `(input String)`                  | `String`        |
| `translate`  | `(lang String)`, `(input String)` | `String`        |
| `llm`        | `(prompt String)`, `(input String)`| `String`       |
| `join-lines` | `(lines (List String))`           | `String`        |
| `shell-run`  | `(command String)`                | `CommandResult` |
| `parse-path` | `(path String)`                   | `Path`          |

Text is just `String`; there are no longer separate `PlainText`/`Markdown`/
`HTML`/`Summary`/`PDF`/`Image` types or `TextLike`/`VisualDoc` aliases, and
there is no `ocr` tool.

## Tool-call syntax

Tool calls are positional: `(tool name arg1 arg2 …)`. There is no `:key value`
keyword-argument syntax for tool calls. `define`, `declare tool`, `retry`, and
`catch` retain their own keyword syntax for their clauses; plain `(tool …)`
expressions pass arguments positionally.

## What `native_dispatch` returns

An `Arc`-shared async closure per tool name.

- `read-file` and `write-file` hit the real filesystem via
  `tokio::fs::read` / `write`.
- `summarize`, `translate`, and `llm` are **mock implementations** that
  return placeholder strings (`[SUMMARY of N chars]`,
  `[TRANSLATED to <lang>]\n…`, etc.). They are sufficient to exercise every
  language construct end-to-end.
- `join-lines` is a pure combinator: it joins a `(List String)` into a
  single `String` separated by newlines. Included primarily to exercise
  `(List T)` at both check and runtime boundaries.
- `shell-run` runs a command via `tokio::process` and returns a
  `CommandResult`.
- `parse-path` normalises a path string into a `Path` value.

To wire real models, replace the corresponding closure in `src/tools.rs`.

## Pipeline position

```
Registry (empty)
     ↓  agnes-builtins::register_builtins   ← you are here
Registry (types, tool signatures ready to check + compile)

Compiled Dag
     ↓  agnes-runtime uses native_dispatch  ← and here
Values
```

## Design notes

- `ToolImpl` is intentionally simple: `HashMap<String, Value>` in, one
  `Value` out. The runtime handles argument binding, boundary validation,
  and error wrapping around this contract.
- Validators are structural, not semantic. They enforce shape (is this a
  UTF-8 string? does it parse as JSON?), not meaning.

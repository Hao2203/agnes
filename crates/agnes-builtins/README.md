# agnes-builtins

MVP built-ins: types (with validators), aliases, and native tool
implementations. This is the crate that turns "the language exists" into
"you can run something useful".

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

**Types + validators (13):**

| Type        | Validator                                    |
|-------------|----------------------------------------------|
| `Path`      | non-empty JSON string, no NUL byte           |
| `PlainText` | valid UTF-8 string                           |
| `Markdown`  | UTF-8 string                                 |
| `HTML`      | UTF-8 string                                 |
| `JSON`      | UTF-8 string parseable as JSON               |
| `PDF`       | byte array starting with `%PDF`              |
| `Image`     | byte array with PNG / JPEG / GIF / WebP magic|
| `Summary`   | UTF-8 string                                 |
| `Unit`      | JSON `null` or `{}`                          |
| `Unknown`   | no validator                                 |
| `String`, `Int`, `Bool` | (no validators; used for literals) |

**Aliases (2):**

- `TextLike = PlainText | Markdown | HTML`
- `VisualDoc = PDF | Image`

**Tools (6):**

| Tool         | Requires                                              | Provides    |
|--------------|-------------------------------------------------------|-------------|
| `read-file`  | `path: Path`                                          | `PlainText` |
| `write-file` | `path: Path`, `content: TextLike`                     | `Unit`      |
| `summarize`  | `input: TextLike | PDF`                               | `Summary`   |
| `translate`  | `input: TextLike`, `lang: String`                     | `PlainText` |
| `ocr`        | `source: VisualDoc`                                   | `PlainText` |
| `llm`        | `prompt: String`, `input: PlainText`                  | `PlainText` |

## What `native_dispatch` returns

An `Arc`-shared async closure per tool name.

- `read-file` and `write-file` hit the real filesystem via
  `tokio::fs::read` / `write`.
- `summarize`, `translate`, `ocr`, `llm` are **mock implementations** that
  return placeholder strings (`[SUMMARY of N chars]`, `[TRANSLATED to <lang>]\n...`,
  etc.). They are sufficient to exercise every language construct end-to-end.

To wire real models or real OCR, replace the corresponding closure in
`src/tools.rs`.

## Pipeline position

```
Registry (empty)
     ↓  agnes-builtins::register_builtins   ← you are here
Registry (types, aliases, tool signatures ready to check + compile)

Compiled Dag
     ↓  agnes-runtime uses native_dispatch  ← and here
Values
```

## Design notes

- `ToolImpl` is intentionally simple: `HashMap<String, Value>` in, one
  `Value` out. The runtime handles kwarg binding, boundary validation,
  and error wrapping around this contract.
- Validators are structural, not semantic. They enforce shape (is this a
  UTF-8 string? does it start with `%PDF`?), not meaning.

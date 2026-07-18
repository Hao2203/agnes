# agnes-parser

S-expression parser for the agnes DSL. Wraps the [`lexpr`] crate and walks
its sexpr tree into `agnes_ast` types.

## Public API

```rust
pub fn parse(source: &str) -> Result<agnes_ast::Program, ParseError>;
pub struct ParseError { pub span: agnes_ast::Span, pub message: String }
```

`ParseError` implements `Display` (renders as
`Parse error at bytes N..M: <message>`) and `std::error::Error` via
`thiserror`.

## What it accepts

A `.agnes` file is a sequence of top-level forms. Any form whose head is
`declare` or `define` becomes a `TopLevel`; anything else at the top level
is the *main* expression. At most one main expression is allowed — put a
`(pipe ...)` around multiple steps.

**Top-level forms:**

```lisp
(declare type PDF)
(declare type-alias TextLike (PlainText | Markdown | HTML))
(declare tool ocr :requires [(source: (PDF | Image))] :provides PlainText)
(define greet :params [(who: PlainText)] :provides PlainText
  (tool llm :prompt "hello" :input who))
```

**Expressions:**

`tool`, `pipe`, `par`, `let` (1-arg and 2-arg forms), `if`, `match`,
`foreach`, `retry`, `catch`, `llm`, `return`, literals, variables.

Keyword arguments use the `:key value` syntax; unions in type expressions
use `|` between members inside a parenthesized list.

## Pipeline position

```
.agnes source
     ↓  agnes-parser  ← you are here
   agnes-ast::Program
     ↓
   agnes-registry / agnes-checker / agnes-compiler
```

## Design notes

- The parser is thin: `lexpr` tokenizes; this crate walks the resulting
  `lexpr::Value` tree.
- Spans are byte offsets and are best-effort: they identify the enclosing
  form via a delimiter-walking heuristic. Sufficient for MVP error output;
  a precise-offset rewrite would replace `form_len_heuristic`.
- Keyword recognition is done via a manual `starts_with(':')` check on
  symbols, because `lexpr` 0.2 doesn't consistently expose `as_keyword` in
  every code path. See the crate doc comment for details.
- The parser rejects a file with multiple main expressions — wrap them in
  a `(pipe ...)` or `(par ...)`.

## Tests

Integration tests live under `tests/parse.rs` and cover:

- happy-path pipe, declare-*, define, both `let` forms
- an unclosed-paren error path

Run: `cargo test -p agnes-parser`.

[`lexpr`]: https://crates.io/crates/lexpr

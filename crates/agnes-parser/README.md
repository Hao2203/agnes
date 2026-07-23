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
(declare type JSON)
(declare type-alias Name (| String Path))
(declare tool translate :requires [(lang String) (input String)] :provides String)
(declare tool join-lines :requires [(lines (List String))] :provides String)
(define greet :params [(who String)] :provides String
  (tool translate "ja" who))
```

**Expressions:**

`tool`, `pipe`, `par`, `let` (1-arg and 2-arg forms), `if`, `match`,
`foreach`, `retry`, `catch`, `return`, `finish`, `observe`, list literals
`[e1 e2 ...]`, literals, variables.

`llm` is **not** a special form - it is an ordinary tool reached through
`(tool llm prompt input)`. `finish` and `observe` are special forms that
wrap a value (or thread the piped upstream value when used bare in a pipe).

Tool calls pass arguments positionally: `(tool name arg1 arg2 …)` - there
is no `:key value` keyword-argument syntax for tool calls. The `:key value`
syntax is used only by the special forms `define`, `declare tool`, `retry`,
and `catch` for their own clauses. Type expressions are themselves S-exprs:
`Named` types are bare symbols (`String`), unions use prefix `|`
(`(| A B C)`), and container types use head-first application (`(List T)`,
`(Option T)`). Param and requires lists use the prefix-name form
`(name Type)` — no colon before the type.

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

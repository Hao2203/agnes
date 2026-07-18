# agnes

A Lisp-style DSL and Rust runtime for LLM-planned agent workflows, with a
TypeScript-style semantic type system that lets LLMs annotate untyped
tools (MCP / CLI / HTTP) and get compile-time and runtime type safety.

**Status:** MVP — proves the language design. Ships 7 built-in tools and
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
  :params  [(path Path) (target String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(pipe
  (let ja (tool read-and-translate :path "README.md" :target "ja"))
  (tool join-lines :lines [ja ja]))
```

## License

MIT OR Apache-2.0

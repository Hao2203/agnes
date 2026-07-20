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

## Interactive chat (agent loop)

Set an API key and:

    ANTHROPIC_API_KEY=... cargo run -p agnes-cli -- chat --llm-provider anthropic

Each natural-language turn drives a multi-iteration agent loop:

1. LLM emits a DSL program.
2. Runtime executes it. If the result is wrapped as `(Observation _)` (via
   the new `observe` tool), the rendered result feeds back to the LLM and
   the loop continues.
3. If the result is `(Finish _)` (via the `finish` tool) or any plain
   type, it's shown to the user and the turn ends.
4. Loop is bounded by `--max-turns <N>` (default 20).

Ctrl-C during a turn cancels the current loop and returns to the prompt.
`/run <dsl>` injects a hand-written DSL as iteration 0; `/history` shows
past turns and their iterations; `/reset` clears history. See
[examples/chat-demo.md](examples/chat-demo.md).

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

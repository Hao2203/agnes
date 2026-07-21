# Interactive `agnes chat` demo

`agnes chat` runs a multi-turn agent loop. Each user turn drives an LLM
that emits agnes DSL; the runtime executes the DSL; if the result is
wrapped in `Observation _`, the observation is fed back and the LLM
continues; if the result is `Finish _` or any other unwrapped type, the
turn ends.

## Quick start (missing key)

```
$ env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u AGNES_LLM_PROVIDER \
  cargo run -p agnes-cli -- chat
```

Expected stderr (anyhow prepends `Error: ` to the first line):

```
Error: Missing provider selection.
  Why: neither the CLI flag `--llm-provider` nor the env var `AGNES_LLM_PROVIDER` is set.
  Fix: pass --llm-provider, set AGNES_LLM_PROVIDER, or add it to .env.
```

Exit status: non-zero.

## Quick start (real key)

```
$ ANTHROPIC_API_KEY=... cargo run -p agnes-cli -- \
    --llm-provider anthropic --llm-model claude-haiku-4-5 chat
```

Then in the REPL:

```
agnes chat — type your goal, or /run <dsl>, /history, /reset, /quit

> read the README and summarize it in one sentence

─── iteration 0 ─────────────────────────────
━━━ Planning ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━
(finish (tool summarize :input (tool read-file :path "README.md")))
━━━ Plan ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
finish   → (Finish Summary)
└── summarize → Summary
    └── read-file → PlainText
━━━ Trace ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
[+0.043s] ▶ read-file :path=README.md
[+0.081s] ✔ read-file (38ms) → PlainText: <content>…
[+0.083s] ▶ summarize :input=<from read-file>
[+1.410s] ✔ summarize (1327ms) → Summary: agnes is a…
agnes is a Rust runtime for a small typed workflow DSL.
```

Bare `finish` as a pipe tail is equivalent shorthand — both of these
produce the same DAG:

```agnes
(finish (tool summarize :input (tool read-file :path "README.md")))
(pipe (tool read-file :path "README.md") (tool summarize) finish)
```

## `observe` example (agent decides to look before speaking)

```
> summarize the README, but only if it's less than 4000 chars

─── iteration 0 ─────────────────────────────
━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━
(observe (tool read-file :path "README.md"))
[+0.081s] ↓ observed (iter 0, 3200 chars): # agnes …

─── iteration 1 ─────────────────────────────
━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━
(finish (tool summarize :input "…"))
[+1.410s] ✔ summarize (1327ms) → Summary: agnes is a…
agnes is a Rust runtime for a small typed workflow DSL.
```

## `--max-turns`

```
$ cargo run -p agnes-cli -- chat --max-turns 5
```

Cap the loop at 5 iterations per turn. On exhaustion:

```
━━━ Turn Failed ━━━━━━━━━━━━━━━━━━━━━━━━━━━
Agent loop hit the iteration limit.
  Why: `MAX_TURNS = 5` reached without a terminating iteration (finish or unlabeled result).
  Fix: rephrase the request more narrowly, or pass `--max-turns <N>` to raise the ceiling.
```

## Mocked built-in tools

Note: this build ships in-memory mocks for the I/O-adjacent tools. See
[`crates/agnes-builtins/src/tools.rs`](../crates/agnes-builtins/src/tools.rs)
for `MOCK_README`, `MOCK_NOTES`, `MOCK_DRAFT` — the strings `read-file`
returns for well-known paths. `write-file` records to a process-global
`writes()` log, drained per turn as `WriteSummary`. `ocr` returns fixed
placeholder text. `llm`, `summarize`, `translate` use the real Provider.

## Manual verification checklist (pending user verification)

- [ ] Missing-key path prints the What/Why/Fix block above and exits non-zero.
- [ ] Real-key path executes translate/summarize with visible plan tree and per-node trace.
- [ ] Two-iteration `observe → finish` path shows both iterations on stderr with the observation line in between.
- [ ] Error-observation path (LLM emits a broken DSL) recovers in a subsequent iteration.
- [ ] `--max-turns 2` for a "loop forever with observe" prompt correctly hits `TurnLimitExceeded`.
- [ ] Ctrl-C during a long turn prints `(cancelled after N iteration(s))` and returns to the prompt.
- [ ] `/history` shows nested iterations with the observation `type` labels.

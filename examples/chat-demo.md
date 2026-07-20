# agnes chat — end-to-end demo

`agnes chat` is an interactive REPL that turns natural language into an
agnes DSL program per turn. The LLM plans; the runtime executes; the
CLI prints a plan tree and per-node trace as tools fire.

## Quick start

Missing-key path (no provider selected):

```bash
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u AGNES_LLM_PROVIDER \
    cargo run -p agnes-cli -- chat
```

Expected stderr (exit code non-zero):

```
Missing provider selection.
  Why: neither the CLI flag `--llm-provider` nor the env var `AGNES_LLM_PROVIDER` is set.
  Fix: pass --llm-provider, set AGNES_LLM_PROVIDER, or add it to .env.
```

Real-key path:

```bash
ANTHROPIC_API_KEY=... cargo run -p agnes-cli -- chat \
    --llm-provider anthropic --llm-model claude-haiku-4-5
```

## About the "tools"

Only `llm` reaches out to a network model. Every other built-in tool —
`read-file`, `translate`, `summarize`, `join-lines`, `write-file`,
`count-lines` — is an **in-memory mock** that returns pre-baked strings
so the demo is deterministic and offline-friendly for tools other than
the planner itself. See
[`crates/agnes-builtins/src/tools.rs`](../crates/agnes-builtins/src/tools.rs)
for the mock corpus (`MOCK_README`, `MOCK_NOTES`, `MOCK_DRAFT`).

The takeaway is that the demo showcases the *planner + runtime + type
system + trace* loop end-to-end. Swap the mock tool bodies for real I/O
and the same DSL programs will run against real filesystems / HTTP
endpoints.

## Manual verification checklist (pending user verification)

The subagent that generated this doc cannot run an interactive TTY with
a real API key. To confirm end-to-end behaviour, a human runs:

```bash
ANTHROPIC_API_KEY=... cargo run -p agnes-cli -- chat \
    --llm-provider anthropic --llm-model claude-haiku-4-5
```

At the `agnes>` prompt, enter each of the following in order:

1. `Translate the readme into Japanese`
2. `now do English too and join them`
3. `/run (tool llm :prompt "haiku about types" :input "")`
4. `/history`
5. `/quit`

Expected per turn:

- The CLI prints a **plan tree** (indented `NodeKind` sketch of the DSL
  the LLM produced) on stderr, then a **per-node trace** with
  `node_start` / `node_end` lines and elapsed time.
- The final result value prints on stdout.
- Turn 2 should reuse or re-emit the `read-and-translate` define pattern
  from turn 1 and produce a joined string.
- Turn 3 (the `/run` slash-command) bypasses the planner and executes
  the hand-written `(tool llm ...)` directly — expect a haiku.
- `/history` dumps the prior user prompts and assistant DSL replies.
- Two `translate` `node_end` lines across the session should show
  elapsed >200ms — that is real API latency, since `translate` itself
  calls into `llm` under the hood.
- `/quit` exits the REPL cleanly (exit code 0).

Cancel signals: `Ctrl-C` cancels the current input line but keeps the
REPL alive; `Ctrl-D` on an empty line quits.

## Example transcript

*Placeholder — pending manual verification per the checklist above. Once
a human runs the session, paste the plan tree + trace here.*

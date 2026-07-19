# Design — Interactive `agnes chat`: real LLM planner + tool-layer LLM + mocked I/O tools

**Date:** 2026-07-19
**Status:** Design approved; ready for implementation plan.

## Context

The agnes MVP proves the language design (parser / type-checker /
DAG compiler / runtime) using **seven built-in tools whose bodies
are placeholders** — `llm` / `summarize` / `translate` / `ocr`
return canned strings. There is no way to see the system do actual
LLM work, and no way to see the "workflow plan and call chain" the
MVP spec talks about — the CLI only prints the final result of a
hand-written `.agnes` file.

The existing MVP spec's LLM Planner (NL → DSL) is a Phase MVP+2 item
that hasn't been built yet.

**Ask:** build an interactive chat CLI where an LLM generates the
DSL from a natural-language turn, the mock tools stand in for
anything that shouldn't require external services, and the user can
clearly see the plan and the call chain. The user can also inject a
hand-written DSL via `/run` in the same session. Write the CLI on
top of a **headless session core** so a GUI can be swapped in later
without moving logic around.

**Intended outcome:** `agnes chat` opens a REPL. Each NL turn is
sent to a real LLM which returns an agnes DSL program; the DSL is
parsed / type-checked / compiled / executed; the user sees
(1) the generated DSL, (2) the compiled plan tree, (3) a live
per-node trace, (4) the final result. Turns share context so the
planner can build on prior results.

## Architecture

```
                      user NL turn
                           │
                           ▼
        ┌──────────────────────────────────┐
        │            agnes-session          │
        │  ┌────────────────────────────┐   │
        │  │ Planner (real LLM call)    │   │  ← agnes-llm
        │  │   sys: tool sigs + few-shot│   │
        │  │   user: turn + history     │   │
        │  └───────┬────────────────────┘   │
        │          │ agnes source           │
        │          ▼                        │
        │  parser → checker → compiler ────┼── on error, feed error
        │          │                        │   back to planner (≤ 2 retries)
        │          ▼                        │
        │  runtime (Tracer → events)        │
        │          │                        │
        │  ┌───────┴───────┐                │
        │  ▼               ▼                │
        │ real LLM   mocked I/O             │
        │ tools      tools                  │
        └──────────┬───────────────────────┘
                   │ SessionEvent stream (EventSink)
                   ▼
    ┌──────────────┴──────────────┐
    │                             │
 agnes-cli                   agnes-gui (future)
 StderrEventSink             ChannelEventSink
 rustyline REPL              windowed UI
```

Two LLM roles (planner + tool executor), one provider abstraction,
one headless session core, swap frontends.

## Design

### 1. New crate `agnes-llm` — provider abstraction

```rust
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<String, LlmError>;
}

pub struct CompletionRequest {
    pub system: Option<String>,
    pub messages: Vec<Message>,   // {role: User|Assistant, content: String}
    pub max_tokens: u32,
}
```

Implementations:

- **`AnthropicProvider`** — Messages API. Headers: `x-api-key`,
  `anthropic-version: 2023-06-01`. Body:
  `{model, max_tokens, system, messages}`.
- **`OpenAiCompatProvider`** — Chat Completions shape. One
  implementation covers OpenAI + DeepSeek + 火山方舟豆包 +
  阿里百炼 via a `base_url` override.

Deps: `reqwest` with `rustls-tls` (no openssl), `serde`,
`serde_json`, `async-trait`, `thiserror`, `tokio`.

**Factory** `Provider::resolve(cli: &LlmCliOpts) -> Result<Arc<dyn Provider>>`
resolves in precedence order **CLI flag → env → `.env`**:

- `--llm-provider` / `AGNES_LLM_PROVIDER` — `anthropic` | `openai`
- `--llm-model` / `AGNES_LLM_MODEL`
- `--llm-base-url` / `AGNES_LLM_BASE_URL` (openai-compat only)
- key: `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` (env / `.env` only,
  never a flag — avoids leaking into shell history)

`.env` loaded at CLI startup via `dotenvy::dotenv()` (silent when
the file is absent). Missing key or missing provider selection →
hard error whose message names the exact env var to set, matching
the LLM-friendly What/Why/Fix template from MVP spec §2.5.
**No silent fallback to mocks.**

### 2. Rewire the three LLM built-ins

`agnes-builtins`: `native_dispatch()` becomes
`native_dispatch(provider: Arc<dyn Provider>) -> HashMap<..>`.

- **`llm`** — `system: None`, one user message
  built by concatenating: `{prompt}\n\n{input}` (with `input`
  omitted from the string when the caller didn't supply `:input`).
- **`summarize`** — `system = "You are a concise summarizer.
  Return one paragraph."`, user = `"Summarize the following:\n\n{input}"`.
- **`translate`** — `system = "You are a professional translator."`,
  user = `"Translate to {lang}. Output only the translation.\n\n{input}"`.

Return wrapped as
`Value::typed(JsonValue::String(text), "PlainText" | "Summary")`.

### 3. Mock the non-LLM built-ins

All I/O-adjacent tools stop touching the real world so the demo is
self-contained:

- **`read-file`** — `HashMap<&str, &str>` seeded with three keys
  (`"README.md"`, `"NOTES.md"`, `"draft.md"`), each holding a
  short canned English paragraph about agnes. Unknown paths return
  a fixed placeholder `"[MOCK file at {path}: agnes is a
  Lisp-style DSL..."` — never errors, so planner-produced paths
  can't accidentally break the demo. No `tokio::fs`.
- **`write-file`** — no disk write. Appends `(path, len)` to a
  process-shared `Mutex<Vec<(String, usize)>>`; the sink surfaces
  the list at end-of-turn.
- **`ocr`** — always returns one fixed English sentence.
- **`join-lines`** — keep the existing real implementation (pure
  string logic, nothing to mock).

### 4. Planner (NL → DSL)

New in `agnes-llm`:

```rust
pub struct Planner {
    provider: Arc<dyn Provider>,
    system: String,        // tool-sig catalogue + syntax rules + few-shot
    history: Vec<Turn>,    // {user_nl, assistant_dsl, result_preview}
}

impl Planner {
    pub async fn plan(&mut self, nl: &str) -> Result<String, PlannerError>;
    pub fn record_result(&mut self, dsl: String, result_preview: String);
    pub fn push_error_feedback(&mut self, bad_dsl: String, err: String);
}
```

**System prompt built at startup** from `Registry` — enumerates every
tool signature (`name :: (req_name Type)... → Provides`) and the
compact set of agnes DSL forms (`pipe`, `par`, `let`, `tool`,
`define`, `if`, `match`, `retry`, `catch`, list literals). Includes
2–3 few-shot examples mirroring `examples/*.agnes`.

**Per-turn message chain**:
`[user0, assistant0(dsl), user1, assistant1(dsl), ...]` — capped
at the **last 6 (user, assistant) pairs = 12 messages**; older pairs
collapsed into a single "prior context" line
(`"<prior turn: user asked X, produced N-line DSL, result was Y-char PlainText>"`)
prepended to the system message.

**Retry loop** (up to 2 corrections — i.e. attempt 0 plus two
retries, so at most 3 planner calls total per turn):

```rust
for attempt in 0..=2 {
    let dsl = planner.plan(nl).await?;
    match parse_check_compile(&dsl, reg) {
        Ok(dag) => return Ok((dsl, dag)),
        Err(e) if attempt < 2 => planner.push_error_feedback(dsl, e.to_string()),
        Err(e) => return Err(e.into()),
    }
}
```

`push_error_feedback` appends `assistant(bad_dsl)` then
`user("That failed with: <error>. Fix and try again; output only the corrected DSL.")`
to `history` for the next `plan()` call.

**DSL extraction** — the planner is instructed to wrap its output
in a ` ```agnes ... ``` ` fenced block; extractor takes the fenced
content. If no fence is found, the raw response is passed through
and the parser produces the error the retry loop feeds back.

### 5. Runtime tracer

New in `agnes-runtime`:

```rust
pub trait Tracer: Send + Sync {
    fn node_start(&self, id: NodeId, kind: &NodeKind, args_preview: &str);
    fn node_end(&self, id: NodeId, result: Result<&Value, &RuntimeError>, elapsed: Duration);
}
pub struct NoopTracer;
pub async fn execute_with(
    dag: &Dag, reg: &Registry, dispatch: &HashMap<..>, tracer: &dyn Tracer,
) -> Result<Value, RuntimeError>;
// existing `execute()` = `execute_with(..., &NoopTracer)` — every
// current test stays intact.
```

Threaded through `scheduler::run` and `eval_node`; hooks fire on
`NodeKind::Tool` and `NodeKind::Llm` only (control-flow nodes stay
silent to keep the trace focused on real work).

### 6. Session core — headless engine (`agnes-session`)

All interactive logic lives in a new crate `agnes-session` that
knows nothing about terminals, colors, or line editors. The CLI is
a thin frontend that drives this crate; a future GUI is another
frontend driving the same API.

```rust
// crates/agnes-session/src/lib.rs
pub struct Session {
    provider: Arc<dyn Provider>,
    registry: Registry,
    dispatch: HashMap<String, ToolImpl>,
    planner: Planner,
}

pub enum TurnInput {
    NaturalLanguage(String),   // → planner
    RawDsl(String),            // → skip planner (used by `/run`)
}

pub enum SessionEvent {
    PlannerStart,
    PlannerRetry { attempt: u8, error: String },
    DslProduced { source: String },
    PlanReady   { tree: PlanTree },              // structured, not pre-rendered
    NodeStart   { id: NodeId, kind: NodeKindTag, args: Vec<(String, String)> },
    NodeEnd     { id: NodeId, ok: bool, preview: String, elapsed_ms: u64 },
    TurnResult  { value_preview: String, value_type: String },
    TurnFailed  { error: String },
}

#[async_trait::async_trait]
pub trait EventSink: Send {
    async fn emit(&mut self, ev: SessionEvent);
}

impl Session {
    pub fn new(provider: Arc<dyn Provider>) -> Result<Self, SessionError>;
    pub async fn run_turn<S: EventSink>(&mut self, input: TurnInput, sink: &mut S)
        -> Result<Value, SessionError>;
    pub fn history(&self) -> &[Turn];
    pub fn reset_history(&mut self);
}
```

`run_turn` orchestrates the full pipeline (planner → parse → check
→ compile → execute) and pushes `SessionEvent`s into the sink as it
goes. The runtime `Tracer` is bridged by a small internal adapter
that converts `Tracer` callbacks into `NodeStart` / `NodeEnd`
events on the same sink.

`PlanTree` is a plain data structure
(`{kind: String, label: String, provides: Option<String>, children: Vec<PlanTree>}`)
— no ANSI, no drawing chars. Frontends render it however they like.

**Two frontends built on this:**

- **`agnes-cli`** — implements a `StderrEventSink` that renders each
  event with `▶` / `✔` / `✘` glyphs, an indent-tree PlanTree
  printer, and 60-char truncation for arg / result previews. Uses
  `rustyline` for the outer REPL loop. Zero business logic in the
  CLI beyond "parse args, build a `Session`, loop reading input,
  feed to session, render events."
- **Future `agnes-gui`** (not built now) — would implement e.g. a
  `ChannelEventSink` that forwards events to a UI thread as
  strongly-typed structs.

### 7. Interactive CLI (`agnes-cli`)

Split `agnes-cli/src/main.rs`; add `cli.rs`, `chat.rs`,
`sink_stderr.rs`, `plan_view.rs`, `input.rs`.

Commands (via `clap`):

```
agnes                                         # → agnes chat (default)
agnes chat  [--llm-provider ...] [--llm-model ...] [--llm-base-url ...]
agnes run   <file.agnes> [same flags]         # non-interactive, backward-compat
```

REPL sketch:

```
━━━ agnes chat ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
provider: anthropic  model: claude-haiku-4-5-...
type your goal, or `/run <dsl>`, `/history`, `/reset`, `/quit`
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
agnes> Translate the README into Japanese and English and join them.

━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
(pipe
  (par
    (let ja (pipe (tool read-file :path "README.md")
                  (tool translate :lang "ja")))
    (let en (pipe (tool read-file :path "README.md")
                  (tool translate :lang "en"))))
  (tool join-lines :lines [ja en]))
━━━ Plan ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
pipe
├── par
│   ├── let ja ← pipe
│   │           ├── read-file  → PlainText
│   │           └── translate  → PlainText
│   └── let en ← pipe
│               ├── read-file  → PlainText
│               └── translate  → PlainText
└── join-lines                 → PlainText
━━━ Trace ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
[+0.02s] ▶ read-file  :path="README.md"
[+0.02s] ✔ read-file  → PlainText(324B) "# agnes ..."
[+0.03s] ▶ translate  :lang="ja" :input=<PlainText 324B>
[+1.42s] ✔ translate  → PlainText(410B) "# agnes\n\nAgnes は Lisp ..."
[+1.42s] ▶ translate  :lang="en" :input=<PlainText 324B>
[+2.31s] ✔ translate  → PlainText(310B) "# agnes ..."
[+2.32s] ▶ join-lines :lines=[<410B>, <310B>]
[+2.32s] ✔ join-lines → PlainText(722B)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

<final result on stdout>

agnes>
```

**stderr** carries Plan + Trace; **stdout** carries only the final
result — so `agnes run file.agnes > out.txt` keeps the traditional
Unix behavior. Slash commands:

- `/run <dsl>` — parse `<dsl>` as agnes source directly, skip
  planner. Multi-line input supported by a `(`-triggered
  bracket-balanced continuation prompt.
- `/history` — dump the last N turns (nl + dsl + result preview).
- `/reset` — clear planner history.
- `/quit` — exit.

Ctrl-C cancels the current turn (via a `tokio::select!` on a
cancellation token); Ctrl-D exits.

### 8. Backward-compat notes

- **`execute()` signature** — kept unchanged; `execute_with()` is
  additive. Existing runtime tests keep passing.
- **`native_dispatch(provider)` signature change** — the ~2–3
  existing call sites (mostly in acceptance tests) are updated
  one-line each to pass a `MockProvider` from `agnes-llm`.
- **`agnes run <file>`** — preserves the current
  `cargo run -p agnes-cli -- <file>` behavior, so the workspace's
  existing acceptance / example flows keep working.

## Non-goals

- Streaming LLM output (post-MVP).
- Tool-use / function-calling from the provider side (we let the
  provider emit *text*, then parse it as agnes DSL — that's the
  whole point of the language layer).
- Cost / token accounting UI.
- Real filesystem `read-file` in chat mode; that stays available
  via `agnes run <file>` non-interactively if needed.
- Multi-provider fallback / retry with backoff.

## Files touched

- **New crates**
  - `crates/agnes-llm/{Cargo.toml, src/lib.rs, src/provider.rs, src/anthropic.rs, src/openai.rs, src/planner.rs, src/error.rs}`
    — Provider trait + two impls + Planner. Headless, no CLI code.
  - `crates/agnes-session/{Cargo.toml, src/lib.rs, src/session.rs, src/events.rs, src/plan_tree.rs, src/tracer_bridge.rs}`
    — headless engine. Consumers: CLI now, GUI later.
- **New in existing crates**
  - `crates/agnes-cli/src/{cli.rs, chat.rs, sink_stderr.rs, plan_view.rs, input.rs}`
  - `examples/chat-demo.md` (walkthrough of a chat session)
- **Modified**
  - `Cargo.toml` — workspace members `agnes-llm`, `agnes-session`;
    workspace deps `reqwest`, `dotenvy`, `async-trait`, `clap`,
    `rustyline`
  - `crates/agnes-builtins/src/tools.rs` — `native_dispatch` takes
    a `Provider`; mock the four non-LLM tools; three LLM tools
    call the provider
  - `crates/agnes-runtime/src/lib.rs` — add `Tracer`, `NoopTracer`,
    `execute_with`
  - `crates/agnes-runtime/src/scheduler.rs` — thread tracer, hook
    Tool + Llm nodes
  - `crates/agnes-cli/src/main.rs` — clap dispatch to `chat` or
    `run`; build `Session`, hand it a `StderrEventSink`
  - `crates/agnes-cli/Cargo.toml` — add `agnes-session`, `clap`,
    `dotenvy`, `rustyline`
  - Any test site that constructs `native_dispatch()` — thread a
    `MockProvider`

## Reused existing pieces

- `agnes_registry::Registry` + `Registry::define_body` — for plan
  rendering + planner system-prompt catalogue.
- `agnes_types::Value::typed_expr` — wrapping provider strings.
- `agnes_parser::parse` / `agnes_checker::check` /
  `agnes_compiler::compile` — reused verbatim by the planner
  retry loop.
- `agnes_runtime::execute` — kept as a thin wrapper over
  `execute_with(..., &NoopTracer)`.

## Verification

1. `cargo test --workspace` — every existing test passes after the
   mechanical `native_dispatch(provider)` threading.
2. **`agnes-llm` unit tests** with a `MockProvider` that returns
   canonical strings — assert the three LLM tools route correctly
   (right `system` / `prompt` / `input`).
3. **Planner retry test** — `MockProvider` first returns a
   syntactically-broken DSL, then a valid one; verify the retry
   loop feeds the parser error back and succeeds on attempt 2.
4. **Snapshot test** (`insta`) of `StderrEventSink` output for a
   fixed DAG driven by a `MockProvider`.
5. **Manual end-to-end** with a real key:
   ```
   ANTHROPIC_API_KEY=sk-... \
   cargo run -p agnes-cli -- chat \
     --llm-provider anthropic --llm-model claude-haiku-4-5-20251001
   ```
   - `> Translate README into Japanese` → generated DSL, plan tree,
     trace with real translate calls (>500ms each), Japanese output.
   - `> now also translate to English and join them` → planner
     uses the prior turn as context to extend.
   - `/run (tool llm :prompt "haiku about types" :input "")` →
     direct DSL injection, one `llm` node call.
6. **Missing-key path** — `agnes chat` without env var → hard error
   naming `ANTHROPIC_API_KEY`, non-zero exit, no network call.

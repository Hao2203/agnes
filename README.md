# agnes

> ⚠️ **Experimental** — agnes is an MVP proving the language design. APIs
> may change, performance is not tuned, and the built-in tool set is
> minimal. Try it, give feedback, but don't bet production on it yet.

A Lisp-style DSL and Rust runtime for LLM-planned agent workflows.
Agnes separates **planning** from **execution**: the LLM emits a typed
DSL program once, and the runtime executes it deterministically — no
per-tool-call LLM round-trips.

```
You: "read the README, summarize it, and translate to Japanese"

LLM generates DSL (one round-trip):
  (pipe
    (tool read-file "README.md")
    (tool summarize)
    (tool translate "ja"))

Runtime executes all three tools, no further LLM calls.
```

## Installation

### Pre-built binaries

Download the latest binary from [GitHub Releases][releases]:

| Platform | Binary |
|----------|--------|
| Linux x86_64 | `agnes-linux-x86_64` |
| macOS (Apple Silicon) | `agnes-macos-aarch64` |
| Windows x86_64 | `agnes-windows-x86_64.exe` |

[releases]: https://github.com/your-org/agnes/releases

### Build from source

```
cargo install --git https://github.com/your-org/agnes agnes-cli
```

Requires Rust stable (edition 2024).

## Quickstart

### 1. Run a `.agnes` file

```
echo '(tool llm "say hi" "")' > hello.agnes
agnes run hello.agnes
```

### 2. Interactive chat (agent loop)

Set an API key and start a REPL:

```
ANTHROPIC_API_KEY=... agnes chat --llm-provider anthropic
```

```
agnes chat - type your goal, or /run <dsl>, /history, /reset, /quit

> read the README and summarize it in one sentence

─── iteration 0 ─────────────────────────────
━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━
(finish (tool summarize (tool read-file "README.md")))
━━━ Plan ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
finish   -> (Finish String)
└── summarize -> String
    └── read-file -> String
━━━ Trace ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
[+0.043s] ▶ read-file :path=README.md
[+0.081s] ✔ read-file (38ms) -> String
[+1.410s] ✔ summarize (1327ms) -> String: agnes is a…
agnes is a Rust runtime for a small typed workflow DSL.
```

Each natural-language turn drives a multi-iteration agent loop. The LLM
emits DSL, the runtime executes it. If the result is wrapped in
`(Observation _)`, the observed output feeds back and the loop continues
— the LLM can **look before it speaks**. The loop is bounded by
`--max-turns` (default 20).

### 3. Key commands

| Command | What it does |
|---------|-------------|
| `/run <dsl>` | Inject a hand-written DSL as iteration 0 |
| `/history` | Show past turns and their iterations |
| `/reset` | Clear conversation history |
| `/quit` | Exit |
| Ctrl-C | Cancel current turn, return to prompt |

## Language at a glance

Agnes programs are S-expressions. There are two layers:

- **Top-level directives** declare types, type aliases, and tool
  signatures — they don't execute.
- **Expression forms** compose tool calls into workflows — they execute.

```lisp
;; ── Top-level ──────────────────────────────
(declare type PDF)
(declare type Markdown)
(declare type-alias TextLike (PlainText | Markdown | HTML))

(declare tool ocr
  :requires [(source (PDF | Image))]
  :provides PlainText)

(define read-and-translate
  :params  [(path Path) (target String)]
  :provides String
  (pipe
    (tool read-file path)
    (tool translate target)))

;; ── Expression ─────────────────────────────
(pipe
  (par
    (let ja (tool read-and-translate "README.md" "ja"))
    (let en (tool read-and-translate "README.md" "en")))
  (tool join-lines [ja en]))
```

| Form | Purpose |
|------|---------|
| `pipe` | Sequential pipeline — output of each step flows to the next |
| `par` | Parallel branches — independent sub-workflows |
| `let` | Name a value (transparent or side-line binding) |
| `tool` | Call a tool by name |
| `define` | Compose tools into a reusable compound tool |
| `if` / `match` | Conditional branching |
| `foreach` | Iterate over a collection |
| `retry` | Retry a step on failure |
| `catch` | Catch errors with a fallback |
| `observe` | Pause execution, feed result back to LLM for re-planning |
| `finish` | Terminate the turn with a final answer |

## How it works

```
User input: "translate the README to Japanese"
    │
    ▼
┌─────────────────────────────────────────────┐
│ 1. LLM PLANNING (one round-trip)            │
│    Natural language → agnes DSL program     │
│    (pipe (tool read-file "README.md")       │
│          (tool translate "ja"))             │
└─────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────┐
│ 2. PARSER                                   │
│    S-expression → AST                       │
└─────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────┐
│ 3. CHECKER                                  │
│    Semantic type-checking:                  │
│    - Does read-file's output satisfy        │
│      translate's input?                     │
│    - Type errors → fix template for LLM     │
└─────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────┐
│ 4. COMPILER                                 │
│    AST → DAG (cycle-free by construction)   │
└─────────────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────────────┐
│ 5. RUNTIME                                  │
│    Execute DAG deterministically:           │
│    read-file → translate → result           │
│    No LLM calls during execution.           │
└─────────────────────────────────────────────┘
    │
    ▼
  Result: "アグネスは..."
```

The key insight: **the LLM plans once, the runtime executes everything**.
Tool calls during execution are deterministic — no per-step LLM
consultation, no "what should I do next?" prompts. This is the
fundamental difference from traditional agent architectures.

## Why agnes? Comparison with traditional agents

### The LLM round-trip problem

Traditional agent frameworks (LangGraph, AutoGPT, direct LLM+tool loops)
follow a **plan-execute-observe-decide** cycle for every single tool
call:

```
Traditional agent:
  LLM → "call read_file" → tool_result → LLM → "call summarize" → tool_result → LLM → "call translate" → tool_result → LLM → answer
         ↑ round-trip 1                  ↑ round-trip 2                  ↑ round-trip 3                  ↑ round-trip 4

Agnes:
  LLM → "(pipe (tool read-file …) (tool summarize …) (tool translate …))" → runtime executes all 3 → answer
         ↑ round-trip 1                                                                                   ↑ done
```

For a 3-tool workflow, the traditional agent burns **4 LLM calls** (one
per tool + the final answer). Agnes burns **1**. The savings compound
with workflow depth.

| | agnes | LangGraph | Dify / n8n | Direct LLM + tool |
|---|---|---|---|---|
| **LLM round-trips per N-tool workflow** | **1** (plan once) | N+1 (one per tool) | N+1 (one per tool) | N+1 (one per tool) |
| **Orchestration** | DSL text | Python code | Drag-and-drop UI | Prompt text |
| **Type safety** | Compile-time + runtime | None | None | None |
| **LLM-generatable** | ✅ Mechanistic syntax | ❌ Must generate valid Python | ❌ Must operate a UI | ✅ But no validation |
| **Portability** | Plain text file | Python runtime + deps | Platform-locked | Plain text |
| **Reusability** | `define` compound tools | Python functions | Templates | None |
| **Execution cost** | $LLM × 1 | $LLM × (N+1) | $LLM × (N+1) | $LLM × (N+1) |
| **Execution latency** | One plan latency + tool time | N × (latency + tool time) | N × (latency + tool time) | N × (latency + tool time) |

### When agnes shines

- **Multi-step tool chains** — the more tools in sequence, the more
  round-trips you save.
- **Cost-sensitive deployments** — 1 LLM call vs N+1.
- **Latency-sensitive workflows** — pipe executes at tool speed, not LLM
  speed.
- **Type-safe tool composition** — catch type mismatches before
  execution, not after.

### When traditional agents may be better

- **Open-ended exploration** — if you genuinely don't know what the next
  step should be, `observe` lets you pause and re-plan, but a
  traditional agent loop is more natural for unbounded exploration.
- **Single-tool tasks** — if every user request is exactly one tool
  call, the round-trip savings vanish.
- **Already invested in a framework** — agnes is a new language, not a
  library you drop into an existing Python project.

## Advanced usage

### Declaring custom types

```lisp
(declare type Sentiment)
(declare type-alias Score (Positive | Negative | Neutral))

(declare tool analyze-sentiment
  :requires [(text String)]
  :provides Sentiment)
```

The type system is name-based (like TypeScript), not trait-based (like
Rust). LLMs annotate tools with semantic type names, and the checker
validates that outputs flow into compatible inputs.

### Multi-observation agent loop

For tasks that genuinely need mid-execution re-planning, use `observe`:

```
> summarize the README, but only if it's less than 4000 chars

─── iteration 0 ─────
(observe (tool read-file "README.md"))
[+0.081s] ↓ observed: <content, 3200 chars>

─── iteration 1 ─────
(finish (tool summarize "…"))
```

The LLM reads the observation, then decides the next step. This gives
you the flexibility of a traditional agent loop when you need it, while
keeping the efficiency of plan-once-execute-all for the 90% case.

### CLI options

```
agnes chat
  --llm-provider <PROVIDER>   LLM provider (anthropic, openai, …)
  --llm-model <MODEL>         Model name
  --llm-base-url <URL>        Custom API base URL
  --max-turns <N>             Max iterations per turn (default: 20)
  --allow-root <DIR>          Restrict file ops to this directory
  --allow-shell               Enable shell command execution

agnes run <file.agnes>
  [same flags]
```

Provider and model can also be set via environment variables
(`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `AGNES_LLM_PROVIDER`, …) or a
`.env` file.

## License

MIT OR Apache-2.0
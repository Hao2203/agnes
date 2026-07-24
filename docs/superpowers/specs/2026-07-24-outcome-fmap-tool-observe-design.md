# Design: Unified Outcome, `fmap`, `tool_observe`, and Branch-Pruned DSL Feedback

## Overview

Three related changes to the agnes agent loop's observation/feedback subsystem:

1. **`fmap` special form** — explicit functor lift over `Observation T` / `Finish T`, preserving the mode without snapshotting. Enables pipes to continue typefully after an Outcome is produced.
2. **`tool_observe` special form** — the combinator `observe ∘ (tool name args...)`. Runs a named tool, snapshots the result for LLM feedback, wraps in `Observation T`, non-terminal. The pipe continues via `fmap`.
3. **Branch-pruned DSL feedback** — when echoing the previous iteration's DSL back to the LLM, only the actually-executed sub-expressions are rendered (dead `if`/`match` arms are dropped).

Together these let the LLM surface intermediate tool outputs (via `tool_observe`), continue piping through Observations (via `fmap`), and see a faithful execution trace (pruned to executed branches).

## Background

The current agent loop feeds back only the *final* value of an iteration (a single `Observation` or `Finish`). Intermediate tool outputs are traced to the terminal UI (`NodeStart`/`NodeEnd` events) but never reach the LLM. The LLM cannot inspect a mid-pipe value without ending the iteration.

Additionally, `Finish T` and `Observation T` are distinct wrapper types. A conditional `(if c (finish x) (observe y))` has both branches producing different types, which the checker does not union — the if's static type is just the then-branch type. While `classify_root` reads the runtime value's type head and works correctly for top-level conditionals, the type system cannot express "this might be either" for `define` signatures or composition.

## Design Decisions

### 1. Separate `Finish T` and `Observation T` Types (Unchanged)

`Finish T` and `Observation T` remain separate wrapper types (`App { head: "Finish", args: [T] }` / `App { head: "Observation", args: [T] }`). `classify_root` reads the outermost type head to determine terminate-vs-continue — exactly as it does today.

Rationale: the intent label (finish vs observe) is semantically meaningful at the type level. Keeping them separate lets the type system track the intent, and `classify_root` works without a runtime mode field. The checker returns `Finish T` for `finish` and `Observation T` for `observe` — distinct types reflecting distinct intents. For conditional `(if c (finish x) (observe y))`, the runtime correctly classifies the result (the actual branch's type head is read by `classify_root`); the checker's static type for `if`/`match` (taking only the then-branch / last arm) is a pre-existing limitation, not addressed by this design.

### 2. `fmap` — Explicit Functor Lift

New special form `(fmap expr)`.

**Signature:** `Observation T → Observation U` (or `Finish T → Finish U`). Valid only when the upstream is already an Outcome.

**Semantics:** Extract the inner value `T` from the upstream Outcome. Evaluate `expr` with `T` as the upstream (piped) value, producing `U`. Re-wrap `U` in the same Outcome wrapper (preserving the mode — Observation stays Observation, Finish stays Finish). No snapshot.

**Type checking:** When upstream is `Observation T`, the checker matches `T` against `expr`'s requires (the tool operates on the inner value). The result type is `Observation U` where `U` is `expr`'s provides.

**Runtime:** The scheduler extracts `inner.data` and `inner.declared_type`, evaluates `expr` with the inner value as upstream, and wraps the result in the same `App { head, args: [U] }`.

**Example:** `(fmap (tool summarize))` on `Observation String` → extracts `String`, runs `summarize` → `Summary`, wraps in `Observation Summary`.

`fmap` over `Finish T` is included for symmetry and extensibility; in practice `finish` is terminal, so `fmap` is primarily used with `Observation T`.

### 3. `tool_observe` — Combinator for Tool + Observe

New special form `(tool_observe name args...)`.

**Syntax:** `(tool_observe <tool-name> <positional-args...>)` — like `(tool ...)` but with observe semantics. Supports bare pipe form: `(tool_observe)` as a pipe tail uses the upstream value directly (no tool run, just snapshot + wrap).

**Signature:** `T → Observation T`. Non-terminal.

**Semantics:** `tool_observe name args... = observe ∘ (tool name args...)`. Run the named tool with the given arguments (and upstream if in a pipe), snapshot the result (rendered via `show_value`, recorded for LLM feedback), wrap in `Observation T`. Non-terminal — the pipe continues (via `fmap` for subsequent steps).

**Type checking:** Matches the tool's requires against the upstream type (or positional args). The result type is `Observation U` where `U` is the tool's provides.

**Runtime:** Evaluates the tool call, snapshots the result value (pushes `(rendered_text, type_name)` to the per-iteration observation recorder), wraps in `Observation U`.

**Bare form:** `(tool_observe)` as a pipe tail — takes the upstream value, snapshots it, wraps in `Observation T`. Equivalent to a non-terminal `observe`.

**Example:** `(tool_observe read-file "x")` → runs `read-file "x"`, snapshots the file content, returns `Observation String`.

### 4. Terminal Forms: `finish` and `observe` (Unchanged Semantics)

`(finish expr)` and `(observe expr)` remain terminal forms. When the upstream is already an Outcome of the same mode, they *absorb* (extract inner, set mode, snapshot for observe) rather than nesting.

| Form | Upstream Type | Result Type | Behavior |
|---|---|---|---|
| `(observe expr)` | `T` (plain) | `Observation T` | Wrap, snapshot, terminal |
| `(observe)` (bare) | `Observation T` | `Observation T` | Absorb, snapshot inner, terminal |
| `(finish expr)` | `T` (plain) | `Finish T` | Wrap, terminal |
| `(finish)` (bare) | `Finish T` | `Finish T` | Absorb, terminal |

Mode mixing is rejected: `finish` on `Observation T` or `observe` on `Finish T` is a type error.

### 5. Pipe Semantics

The pipe is **uniform**: each step's declared input type must match the previous step's output type. No implicit lifting.

| Phase | Steps | Type Flow |
|---|---|---|
| Plain | `(tool f)`, `(tool g)`, ... | `T → U → V → ...` |
| Transition | `(tool_observe name args...)` | `T → Observation U` |
| Outcome | `(fmap expr)`, `(fmap expr2)`, ... | `Observation U → Observation V → ...` |
| Terminal | `(observe)` or `(finish)` | Ends the pipe |

Bare `(tool_observe)` as a pipe tail (no tool name) is equivalent to a non-terminal `observe`: snapshots the upstream value and wraps in `Observation T`, without ending the pipe. This is useful when the LLM wants to surface a value that was already computed by a previous step, without running an additional tool.

**Example:**
```lisp
(pipe (tool_observe read-file "x")     ;; String → Observation String (snapshot, non-terminal)
      (fmap (tool summarize))          ;; Observation String → Observation Summary (pure fmap, no snapshot)
      (observe))                       ;; Observation Summary → Observation Summary (absorb, snapshot, terminal)
```

Fed back: `[file content, summary]`. Loop continues (Observation mode).

Without the terminal `(observe)`, the pipe ends with `Observation Summary` (Observation mode from `tool_observe`, propagated by `fmap`). The final value is not auto-snapshotted — only the `tool_observe` snapshot (file content) is fed back.

### 6. Snapshot Accumulation and Multi-Observation Feedback

`tool_observe` and `observe` push snapshots to a per-iteration recorder (process-global `Mutex<Vec<ObservationRecord>>`, mirroring the `writes()` pattern in `agnes-builtins`). Each snapshot = `(rendered_text: String, type_name: Option<TypeName>)`.

The session drains the recorder after each iteration:

- **Observation (continue):** drained snapshots are fed back as `<observation>` messages, in execution order. Each becomes a separate user message in `build_messages`.
- **Finish/Other (terminate):** final value is shown to the user; snapshots are discarded (the turn ends, no LLM feedback).

**Planner model change:** `Iteration` now holds `observations: Vec<Observation>` instead of `observation: Option<Observation>`. `push_observation` is replaced by `append_observations`. `build_messages` emits one user message per observation.

Snapshot truncation (`OBSERVATION_TRUNCATION_THRESHOLD`, 8000 chars) is applied to each snapshot individually.

### 7. Branch-Pruned DSL Feedback

When echoing the previous iteration's DSL back to the LLM, only the actually-executed sub-expressions are rendered.

**Mechanism:**
1. **Scheduler** records a `visited: HashSet<NodeId>` during execution. `eval_node` inserts `id` at entry. Since the scheduler only descends into taken branches (`If` → picked branch; `Match` → matched arm), dead branches are naturally excluded.
2. **Lowerer** records `NodeId → Span` mapping during compilation (`lower_program`), enabling AST node lookup from DAG node IDs.
3. **New AST pretty-printer** (`Expr` → source string) walks the AST, and for `If`/`Match` nodes, renders only the visited branch (unwrapping it — the `if`/`match` wrapper is dropped). For `Pipe`, all steps are visited → all rendered. For `Par`, all branches are visited → all rendered. For `Retry`/`Catch`, visited children are rendered.
4. **Session** computes the pruned DSL after execution and passes it to the planner. `build_messages` echoes the pruned DSL instead of the verbatim source. Original source is retained for `/history`.

**Example:**
```lisp
;; Original DSL:
(pipe (tool_observe read-file "x")
      (if (tool is-empty)
          (finish "empty")
          (fmap (tool summarize))))

;; Pruned (is-empty returned false, fmap branch executed):
(pipe (tool_observe read-file "x")
      (fmap (tool summarize)))
```

### 8. System Prompt Updates

The planner's system prompt is updated to document the new special forms:

- `fmap` — "`(fmap X)` lifts expression X over an Outcome. Use after `tool_observe` to continue transforming the observed value without ending the pipe."
- `tool_observe` — "`(tool_observe NAME ARGS...)` runs a tool and surfaces its result to you as an observation, without ending the pipe. Use to inspect intermediate values."

## Error Handling

| Error Case | Behavior |
|---|---|
| `(fmap expr)` on plain upstream | Checker rejects: "fmap requires an Outcome upstream, got T" |
| `(finish)` on `Observation T` | Checker rejects: "cannot finish an Observation" |
| `(observe)` on `Finish T` | Checker rejects: "cannot observe a Finish" |
| `(tool_observe name...)` on `Observation T` upstream | Checker rejects: "tool_observe requires a plain upstream, got Observation T" |
| Tool fails mid-pipe after `tool_observe` | Partial visited set; pruned DSL shows what ran before failure. `tool_observe` snapshots (before failure) are retained and fed back. Error observation is appended. |
| `Retry` with N attempts | Body visited up to N+1 times; rendered once in pruned DSL |
| `Catch` with successful body | Fallback is unvisited, pruned from rendered DSL |

## Backward Compatibility

- Unlabeled DSL (no `finish`/`observe`/`tool_observe`/`fmap`) → `classify_root` returns `Other` → implicit finish. Unchanged.
- `(finish X)` and `(observe X)` syntax and semantics unchanged.
- Existing `examples/*.agnes` and all tests pass without modification.
- `Finish` and `Observation` types remain registered in the registry; `classify_root` reads the type head as before.

## Files Changed

| Crate | File | Change |
|---|---|---|
| `agnes-ast` | `src/lib.rs` | Add `Expr::Fmap` and `Expr::ToolObserve` variants |
| `agnes-parser` | `src/expr.rs` | Parse `(fmap ...)` and `(tool_observe ...)` forms |
| `agnes-checker` | `src/lib.rs` | Type-check `fmap` (Outcome lift) and `tool_observe` (T → Observation T); reject mode mismatches |
| `agnes-compiler` | `src/lower.rs` | Lower `Fmap`/`ToolObserve`; record `NodeId → Span` mapping |
| `agnes-compiler` | `src/dag.rs` | Add `NodeKind::Fmap` and `NodeKind::ToolObserve` |
| `agnes-runtime` | `src/scheduler.rs` | Evaluate `Fmap` (extract, apply, rewrap) and `ToolObserve` (run tool, snapshot, wrap); record visited `NodeId` set; pipe mode propagation |
| `agnes-builtins` | `src/lib.rs` or new module | `observations()` recorder (mirrors `writes()`) |
| `agnes-session` | `src/session.rs` | Drain observations per iteration; compute pruned DSL; pass to planner |
| `agnes-session` | `src/plan_tree.rs` | Optionally update plan tree for new node kinds |
| `agnes-llm` | `src/planner.rs` | `Iteration.observations: Vec<Observation>`; `build_messages` emits multiple observations; update system prompt |
| `agnes-ast` | new `src/display.rs` | AST pretty-printer for pruned DSL rendering |

## Testing Strategy

| Category | What to Test |
|---|---|
| **Type checking** | `tool_observe` produces `Observation T`; `fmap` preserves mode on Outcome; `observe` absorbs on `Observation`; mode mismatches rejected; `fmap` on plain upstream rejected |
| **Runtime** | `tool_observe` snapshots + non-terminal; `fmap` pure lift, no snapshot; pipe sequences correctly; bare `tool_observe` in pipe tail |
| **Agent loop** | Multiple observations fed back in order; Finish discards snapshots; implicit finish unchanged; truncation applied per-observation |
| **Branch pruning** | Dead `if`/`match` arms dropped; `pipe`/`par` kept whole; errored mid-pipe renders partial; `Retry`/`Catch` render visited children |
| **Backward compat** | Existing examples unmodified; unlabeled DSL implicit finish; `finish`/`observe` unchanged |
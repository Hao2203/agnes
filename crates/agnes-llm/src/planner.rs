use crate::error::PlannerError;
use crate::provider::{CompletionRequest, Message, Provider, Role};
use agnes_registry::Registry;
use std::sync::Arc;

const MAX_TURNS_VERBATIM: usize = 6;
const PLANNER_MAX_TOKENS: u32 = 2048;

/// A committed user↔agent turn: user_nl, one or more iterations of DSL
/// (with optional intermediate observations), and a final outcome.
#[derive(Debug, Clone)]
pub struct Turn {
    pub user_nl: String,
    pub iterations: Vec<Iteration>,
    pub outcome: TurnOutcome,
}

/// A single iteration inside a turn: assistant DSL plus any number of
/// observations generated during execution (tool_observe snapshots, plus
/// the final `observe` if applicable).
///
/// `observations` is empty on the final iteration of a finished turn
/// (Finish or implicit finish).
#[derive(Debug, Clone)]
pub struct Iteration {
    pub assistant_dsl: String,
    /// Observations collected during this iteration (tool_observe snapshots
    /// plus the final observe). Empty for finish/implicit-finish iterations.
    pub observations: Vec<Observation>,
    /// Optional pruned DSL for echoing back to the LLM. When present, this
    /// is the DSL that was actually executed after dead branches were
    /// pruned. If None, `assistant_dsl` is used as the echo content.
    pub executed_dsl: Option<String>,
}

/// What the runtime returned during an iteration (Observation branch) or
/// what error was encountered before the next planner call.
#[derive(Debug, Clone)]
pub struct Observation {
    pub text: String,
    pub is_error: bool,
    /// Inner type name (Finish/Observation stripped one layer) for the
    /// `<observation type="...">` XML attribute. `None` on error paths.
    pub type_name: Option<agnes_types::TypeName>,
}

/// How a turn ended.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// Terminating iteration produced a value; `result` is the shown string.
    Finished { result: String },
    /// Session hit MAX_TURNS without a terminating iteration.
    TurnLimitExceeded,
}

struct InflightTurn {
    user_nl: String,
    iterations: Vec<Iteration>,
}

pub struct Planner {
    provider: Arc<dyn Provider>,
    base_system: String,
    history: Vec<Turn>,
    /// In-flight turn state. `None` when no user turn is active.
    inflight: Option<InflightTurn>,
}

impl Planner {
    pub fn new(provider: Arc<dyn Provider>, registry: &Registry) -> Self {
        Self {
            provider,
            base_system: build_system_prompt(registry),
            history: Vec::new(),
            inflight: None,
        }
    }

    /// Read-only view of committed turns.
    pub fn history(&self) -> &[Turn] {
        &self.history
    }

    /// Discard committed history. Does not touch in-flight state; call
    /// `abandon_pending_turn` first if you also want that cleared.
    pub fn reset_history(&mut self) {
        self.history.clear();
    }

    /// Start a new in-flight user turn. Aborts any existing in-flight turn
    /// (with TurnLimitExceeded outcome), so callers must have already
    /// committed or explicitly abandoned prior turns before calling this.
    pub fn begin_user_turn(&mut self, nl: String) {
        // Defensive: if a prior turn is still in-flight, abandon it. In
        // normal flow the Session calls record_finish or abandon_pending_turn
        // before begin_user_turn, so this branch is a safety net.
        if self.inflight.is_some() {
            self.abandon_pending_turn();
        }
        self.inflight = Some(InflightTurn {
            user_nl: nl,
            iterations: Vec::new(),
        });
    }

    /// Ask the LLM for the next DSL iteration. Appends `assistant(dsl)`
    /// to the in-flight iterations (with empty observations until
    /// `append_observations` or `record_finish` is called).
    pub async fn plan_next(&mut self) -> Result<String, PlannerError> {
        let messages = self.build_messages();
        let request = CompletionRequest {
            system: Some(self.effective_system()),
            messages,
            max_tokens: PLANNER_MAX_TOKENS,
        };
        let raw = self.provider.complete(request).await?;
        let dsl = crate::dsl_extract::extract_dsl(&raw);
        if dsl.trim().is_empty() {
            // Surface what the provider actually returned so callers can
            // tell "empty content" from "prose without a fenced block" from
            // "fenced block with empty body". Cap preview to keep TurnFailed
            // events readable in the terminal.
            const PREVIEW_CHARS: usize = 500;
            let raw_len = raw.chars().count();
            let raw_preview = if raw.trim().is_empty() {
                format!("<whitespace-only, {} bytes>", raw.len())
            } else {
                let mut p: String = raw.chars().take(PREVIEW_CHARS).collect();
                if raw_len > PREVIEW_CHARS {
                    p.push('…');
                }
                p
            };
            return Err(PlannerError::EmptyResponse {
                raw_len,
                raw_preview,
            });
        }
        // Append to in-flight.
        let inflight = self
            .inflight
            .as_mut()
            .expect("plan_next called with no in-flight turn (missing begin_user_turn?)");
        inflight.iterations.push(Iteration {
            assistant_dsl: dsl.clone(),
            observations: Vec::new(),
            executed_dsl: None,
        });
        Ok(dsl)
    }

    /// Append observations to the last in-flight iteration. Used for both
    /// tool_observe snapshots (multiple per iteration) and the final
    /// top-level observe. The `_dsl` parameter is kept for call-site
    /// symmetry with `push_observation` / `record_finish`.
    pub fn append_observations(
        &mut self,
        _dsl: String,
        obs: Vec<Observation>,
    ) {
        let inflight = self
            .inflight
            .as_mut()
            .expect("append_observations with no in-flight turn");
        let last = inflight
            .iterations
            .last_mut()
            .expect("append_observations with no iterations (missing plan_next?)");
        last.observations.extend(obs);
    }

    /// Inject a pre-computed assistant DSL (from RawDsl input) into the
    /// in-flight turn as if `plan_next` had produced it. Does not call
    /// the provider. Behaves identically to `plan_next` from the caller's
    /// perspective: the next `append_observations` / `record_finish` will
    /// attach to this synthetic iteration.
    pub fn inject_assistant_dsl(&mut self, dsl: String) {
        let inflight = self
            .inflight
            .as_mut()
            .expect("inject_assistant_dsl with no in-flight turn");
        inflight.iterations.push(Iteration {
            assistant_dsl: dsl,
            observations: Vec::new(),
            executed_dsl: None,
        });
    }

    /// Set the executed (branch-pruned) DSL on the last in-flight
    /// iteration. This is what will be echoed back to the LLM instead
    /// of the raw `assistant_dsl`.
    pub fn set_executed_dsl(&mut self, dsl: String) {
        let inflight = self
            .inflight
            .as_mut()
            .expect("set_executed_dsl with no in-flight turn");
        let last = inflight
            .iterations
            .last_mut()
            .expect("set_executed_dsl with no iterations (missing plan_next?)");
        last.executed_dsl = Some(dsl);
    }

    /// Commit the in-flight turn as Finished. Consumes `inflight`.
    /// The dsl must match the last iteration's assistant_dsl (sanity
    /// check). This invariant is maintained because of inject_assistant_dsl
    /// is always called before record_finish for RawDsl paths.
    pub fn record_finish(&mut self, dsl: String, result: String) {
        let mut inflight = self
            .inflight
            .take()
            .expect("record_finish called without begin_user_turn");
        let last = inflight
            .iterations
            .last_mut()
            .expect("record_finish called before any iteration was recorded");
        assert_eq!(
            last.assistant_dsl, dsl,
            "record_finish dsl must match the last iteration's assistant_dsl"
        );
        // Finish iterations typically have no observations, but we don't
        // assert it — any collected observations are simply not echoed
        // further once the turn commits.
        self.history.push(Turn {
            user_nl: inflight.user_nl,
            iterations: inflight.iterations,
            outcome: TurnOutcome::Finished { result },
        });
    }

    /// Commit the in-flight turn as TurnLimitExceeded. No-op if no
    /// in-flight turn exists.
    pub fn abandon_pending_turn(&mut self) {
        if let Some(inflight) = self.inflight.take() {
            if inflight.iterations.is_empty() {
                // Nothing worth committing to history; drop scratch silently.
                return;
            }
            self.history.push(Turn {
                user_nl: inflight.user_nl,
                iterations: inflight.iterations,
                outcome: TurnOutcome::TurnLimitExceeded,
            });
        }
    }

    fn effective_system(&self) -> String {
        let n = self.history.len();
        if n <= MAX_TURNS_VERBATIM {
            return self.base_system.clone();
        }
        let extras: &[Turn] = &self.history[..n - MAX_TURNS_VERBATIM];
        let mut prefix = String::from("<prior context:\n");
        for t in extras {
            let iters = t.iterations.len();
            let outcome = match &t.outcome {
                TurnOutcome::Finished { result } => {
                    format!("finished ({} chars)", result.chars().count())
                }
                TurnOutcome::TurnLimitExceeded => "turn-limit-exceeded".to_string(),
            };
            prefix.push_str(&format!(
                "  - user asked {:?}: {iters} iteration(s), outcome: {outcome}\n",
                t.user_nl,
            ));
        }
        prefix.push_str(">\n\n");
        prefix.push_str(&self.base_system);
        prefix
    }

    fn build_messages(&self) -> Vec<Message> {
        let mut out = Vec::new();
        let n = self.history.len();
        let start = n.saturating_sub(MAX_TURNS_VERBATIM);
        for turn in &self.history[start..] {
            out.push(Message {
                role: Role::User,
                content: turn.user_nl.clone(),
            });
            for it in &turn.iterations {
                let dsl = it.executed_dsl.as_ref().unwrap_or(&it.assistant_dsl);
                out.push(Message {
                    role: Role::Assistant,
                    content: format!("```agnes\n{}\n```", dsl),
                });
                for obs in &it.observations {
                    out.push(Message {
                        role: Role::User,
                        content: wrap_observation(obs),
                    });
                }
            }
        }
        if let Some(inflight) = &self.inflight {
            out.push(Message {
                role: Role::User,
                content: inflight.user_nl.clone(),
            });
            for it in &inflight.iterations {
                let dsl = it.executed_dsl.as_ref().unwrap_or(&it.assistant_dsl);
                out.push(Message {
                    role: Role::Assistant,
                    content: format!("```agnes\n{}\n```", dsl),
                });
                for obs in &it.observations {
                    out.push(Message {
                        role: Role::User,
                        content: wrap_observation(obs),
                    });
                }
            }
        }
        out
    }
}

fn wrap_observation(obs: &Observation) -> String {
    if obs.is_error {
        format!("<observation error=\"true\">\n{}\n</observation>", obs.text)
    } else {
        match &obs.type_name {
            Some(t) => format!(
                "<observation type=\"{}\">\n{}\n</observation>",
                t.0, obs.text
            ),
            None => format!("<observation>\n{}\n</observation>", obs.text),
        }
    }
}

fn build_system_prompt(reg: &Registry) -> String {
    // Tool catalog: iterate the fixed list of builtin tools in a
    // stable order. Registry does not expose iteration; naming
    // the tools explicitly is a deliberate choice for prompt
    // determinism. NOTE: `finish` and `observe` are special forms,
    // not tools, so they are NOT listed here — they appear in the
    // grammar section below instead.
    const BUILTIN_TOOL_ORDER: &[&str] = &[
        "read-file",
        "write-file",
        "parse-path",
        "summarize",
        "translate",
        "llm",
        "join-lines",
        "shell-run",
    ];
    let mut catalog = String::new();
    for name in BUILTIN_TOOL_ORDER {
        if let Some(sig) = reg.tool_signature(name) {
            catalog.push_str(&format!("  - {} :", name));
            for (pname, pty) in &sig.requires {
                catalog.push_str(&format!(" {pname} {pty}"));
            }
            catalog.push_str(&format!("  ->  {}\n", sig.provides));
        }
    }

    format!(
        r#"You are the planning brain of an agnes agent. Each turn you produce
one agnes DSL expression as an ```agnes fenced block. That expression will
be parsed, type-checked, compiled, and executed by the runtime.

Loop protocol:
  * Wrap your final answer with `(finish X)` to end this user turn — the
    rendered result of `X` is shown to the user and the loop stops.
  * Wrap a value with `(observe X)` when you want to see the result and
    decide the next step — the runtime sends `X` back as a
    `<observation type="T">...</observation>` message on the next turn.
  * If neither wrapper is present, the runtime treats the result as an
    IMPLICIT finish (still shown to the user, turn ends). Prefer the
    explicit `(finish X)` form; unlabeled works but is less clear.
  * On error (parse/check/compile/execute), you receive
    `<observation error="true">...</observation>` and should produce a
    corrected DSL on the next turn.

Grammar cheatsheet — these are the SPECIAL FORMS (not tools):
  * `(finish X)` — terminate this turn with `X` as the result.
  * `(observe X)` — return `X` as an observation, continue the loop.
  * `(fmap X)` — lift expression `X` over an Observation (or Finish).
    Use after `tool_observe` to keep transforming the observed value
    without ending the pipe.
  * `(tool_observe NAME ARGS...)` — run a tool and surface its result
    to you as an observation, without ending the pipe. The pipe
    continues with the tool's result wrapped in Observation.
    `(tool_observe)` as a bare pipe tail snapshots the upstream value.
  * `(pipe e1 e2 ... eN)` — thread each expression's result into the
    next. Bare `finish` / `observe` as a pipe tail is shorthand for
    `(finish <upstream>)` / `(observe <upstream>)`.
  * `(tool NAME arg1 arg2 ...)` — call a tool from the catalog below.
    Args are positional, in the order shown in the catalog. To feed a
    piped value into a parameter, omit that parameter.
  * `(let name expr)` or `(let name)` (inside a pipe) — bind a value.
  * `(if cond then else)`, `(match scrutinee (pattern arm) ...)`,
    `(foreach item collection body)` — control flow.
  * `(list e1 e2 ...)` or `[e1 e2 ...]` — list literals.

Available builtin tools (I/O primitives; use with `(tool NAME ...)`):
{catalog}
Rules:
  1. Produce EXACTLY ONE fenced ```agnes block per turn. No prose outside.
  2. `finish` and `observe` are LANGUAGE FORMS. Write `(finish X)` — do
     NOT write `(tool finish X)`; there is no such tool.
  3. Prefer wrapping every terminating result with `(finish ...)` to
     make your intent explicit.
  4. Do not invent tools not in the catalog above; the checker will reject.
  5. `shell-run` executes an arbitrary shell command and requires the
     `--allow-shell` flag plus per-command user confirmation, so it may be
     rejected at runtime. Prefer `read-file`/`write-file` for file access;
     use `shell-run` only when no builtin tool fits.

Examples (each is a complete turn):

```agnes
(finish "Hello! How can I help you today?")
```

```agnes
(finish (tool summarize (tool read-file "notes.md")))
```

```agnes
(pipe (tool read-file "log.txt") (tool summarize) observe)
```
"#
    )
}

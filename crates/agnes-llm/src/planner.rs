use crate::dsl_extract::extract_dsl;
use crate::error::PlannerError;
use crate::provider::{CompletionRequest, Message, Provider, Role};
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeName};
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

/// A single (assistant DSL, resulting observation) pair inside a turn.
/// `observation.is_none()` on the LAST iteration means that DSL was the
/// terminating one (Finish or implicit).
#[derive(Debug, Clone)]
pub struct Iteration {
    pub assistant_dsl: String,
    pub observation: Option<Observation>,
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
    /// to the in-flight iterations (with observation=None until
    /// `push_observation` or `record_finish` is called).
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
            return Err(PlannerError::EmptyResponse);
        }
        // Append to in-flight.
        let inflight = self
            .inflight
            .as_mut()
            .expect("plan_next called with no in-flight turn (missing begin_user_turn?)");
        inflight.iterations.push(Iteration {
            assistant_dsl: dsl.clone(),
            observation: None,
        });
        Ok(dsl)
    }

    /// Attach an observation to the last in-flight iteration. If the
    /// last iteration already has an observation (double push), that is
    /// a caller bug — we panic loudly.
    pub fn push_observation(
        &mut self,
        _dsl: String,
        text: String,
        is_error: bool,
        type_name: Option<agnes_types::TypeName>,
    ) {
        let inflight = self
            .inflight
            .as_mut()
            .expect("push_observation with no in-flight turn");
        let last = inflight
            .iterations
            .last_mut()
            .expect("push_observation with no iterations (missing plan_next?)");
        assert!(
            last.observation.is_none(),
            "push_observation called twice on the same iteration"
        );
        last.observation = Some(Observation {
            text,
            is_error,
            type_name,
        });
    }

    /// Inject a pre-computed assistant DSL (from RawDsl input) into the
    /// in-flight turn as if `plan_next` had produced it. Does not call
    /// the provider. Behaves identically to `plan_next` from the caller's
    /// perspective: the next `push_observation` / `record_finish` will
    /// attach to this synthetic iteration.
    pub fn inject_assistant_dsl(&mut self, dsl: String) {
        let inflight = self
            .inflight
            .as_mut()
            .expect("inject_assistant_dsl with no in-flight turn");
        inflight.iterations.push(Iteration {
            assistant_dsl: dsl,
            observation: None,
        });
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
        let last = inflight.iterations.last_mut()
            .expect("record_finish called before any iteration was recorded");
        assert_eq!(last.assistant_dsl, dsl,
            "record_finish dsl must match the last iteration's assistant_dsl");
        assert!(last.observation.is_none(),
            "record_finish called on an iteration that already has an observation");
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
                out.push(Message {
                    role: Role::Assistant,
                    content: format!("```agnes\n{}\n```", it.assistant_dsl),
                });
                if let Some(obs) = &it.observation {
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
                out.push(Message {
                    role: Role::Assistant,
                    content: format!("```agnes\n{}\n```", it.assistant_dsl),
                });
                if let Some(obs) = &it.observation {
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
        format!(
            "<observation error=\"true\">\n{}\n</observation>",
            obs.text
        )
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
    // determinism.
    const BUILTIN_TOOL_ORDER: &[&str] = &[
        "read-file",
        "write-file",
        "summarize",
        "translate",
        "ocr",
        "llm",
        "join-lines",
        "finish",
        "observe",
    ];
    let mut catalog = String::new();
    for name in BUILTIN_TOOL_ORDER {
        if let Some(sig) = reg.tool_signature(name) {
            catalog.push_str(&format!("  - {} :", name));
            for (pname, pty) in &sig.requires {
                catalog.push_str(&format!(" :{pname} {pty}"));
            }
            catalog.push_str(&format!("  ->  {}\n", sig.provides));
        }
    }

    format!(r#"You are the planning brain of an agnes agent. Each turn you produce
one agnes DSL expression as an ```agnes fenced block. That expression will
be parsed, type-checked, compiled, and executed by the runtime.

Loop protocol:
  * If your expression's result is wrapped as `(Observation T)` — e.g.
    the outermost tool call is `observe` — the runtime feeds the rendered
    result back to you on the next turn as a `<observation type="T">...</observation>`
    block, and you produce another DSL expression.
  * If the result is wrapped as `(Finish T)` — outer tool is `finish` —
    the rendered result is shown to the user and the turn ENDS.
  * If the result is neither `Finish` nor `Observation` (any plain type),
    the runtime treats it as an IMPLICIT finish: shows to user, turn ends.
    So unlabeled DSL still works; you only *need* `observe` when you want
    to see the result and continue.
  * On error (parse/check/compile/execute), you receive
    `<observation error="true">...</observation>` and should produce a
    corrected DSL on the next turn.

Available builtin tools:
{catalog}
Rules:
  1. Produce EXACTLY ONE fenced ```agnes block per turn. No prose outside.
  2. Prefer `finish` at the tail to make your intent explicit; unlabeled
     is allowed but observability suffers.
  3. Use `observe` when you need to see the result to decide the next step.
  4. Do not invent tools not in the catalog above; the checker will reject.

Examples (each is a complete turn):

```agnes
(pipe (tool read-file :path "notes.md") (tool summarize) finish)
```

```agnes
(pipe (tool read-file :path "log.txt") (tool summarize) observe)
```

```agnes
(pipe "task complete" finish)
```
"#)
}

fn format_sig(sig: &ToolSignature) -> String {
    let params: Vec<String> = sig
        .requires
        .iter()
        .map(|(n, t)| format!("({n} {t})"))
        .collect();
    format!("{} -> {}", params.join(" "), sig.provides)
}

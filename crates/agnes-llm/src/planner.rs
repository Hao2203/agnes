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

    /// Commit the in-flight turn as Finished. Consumes `inflight`.
    /// The dsl arg must equal the last iteration's assistant_dsl (sanity
    /// check); if not, we still commit but stamp a fresh iteration.
    pub fn record_finish(&mut self, dsl: String, result: String) {
        let mut inflight = self
            .inflight
            .take()
            .expect("record_finish with no in-flight turn");
        // If the last iteration's DSL doesn't match, append a synthetic
        // iteration for it. This handles the edge where RawDsl was used
        // (planner never saw plan_next for this DSL).
        let last_matches = inflight
            .iterations
            .last()
            .is_some_and(|it| it.assistant_dsl == dsl);
        if !last_matches {
            inflight.iterations.push(Iteration {
                assistant_dsl: dsl,
                observation: None,
            });
        }
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
        // For now just return base_system. Task 7 will add the "prior context"
        // summary logic.
        self.base_system.clone()
    }

    fn build_messages(&self) -> Vec<Message> {
        let mut out = Vec::new();
        for turn in &self.history {
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

fn build_system_prompt(registry: &Registry) -> String {
    let mut s = String::new();
    s.push_str("You are the agnes DSL planner. Given a user goal, produce an agnes program that achieves it using the registered tools.\n\n");
    s.push_str("Output ONLY an ```agnes fenced code block containing the program — no prose, no explanation.\n\n");
    s.push_str("agnes forms:\n");
    s.push_str("  (pipe expr1 expr2 ...)                 sequential flow; each step's output becomes the next step's implicit input\n");
    s.push_str("  (par branch1 branch2 ...)              parallel branches; each branch's value is discarded (use `let` inside)\n");
    s.push_str("  (let name expr)                        bind expr's value to `name` (or bind the piped-in value if expr omitted)\n");
    s.push_str("  (tool NAME :key value :key value ...)  call a tool; kwargs match the tool's `requires` param names\n");
    s.push_str("  (list e1 e2 ...)                       or bracket literal [e1 e2 ...]\n");
    s.push_str("  (if cond then else) / (match scrutinee (pat arm) ...) / (retry N body) / (catch body fallback)\n");
    s.push_str("  Literals: strings \"...\", ints, true/false, nil.\n\n");
    s.push_str("Registered tools:\n");
    // The registry doesn't expose iteration; we synthesize the catalog by
    // asking for each known tool name in a fixed order. In practice all
    // callers register the 7 builtins, so this list matches.
    for name in [
        "read-file",
        "write-file",
        "summarize",
        "translate",
        "ocr",
        "llm",
        "join-lines",
    ] {
        if let Some(sig) = registry.tool_signature(name) {
            s.push_str(&format!("  {name} :: {}\n", format_sig(sig)));
        }
    }
    s.push('\n');
    s.push_str("Examples:\n\n");
    s.push_str("  goal: read the readme and summarize it\n");
    s.push_str("  ```agnes\n  (pipe (tool read-file :path \"README.md\") (tool summarize))\n  ```\n\n");
    s.push_str("  goal: translate the readme into Japanese and English, then join\n");
    s.push_str("  ```agnes\n  (pipe\n    (par\n      (let ja (pipe (tool read-file :path \"README.md\") (tool translate :lang \"ja\")))\n      (let en (pipe (tool read-file :path \"README.md\") (tool translate :lang \"en\"))))\n    (tool join-lines :lines [ja en]))\n  ```\n");
    s
}

fn format_sig(sig: &ToolSignature) -> String {
    let params: Vec<String> = sig
        .requires
        .iter()
        .map(|(n, t)| format!("({n} {t})"))
        .collect();
    format!("{} -> {}", params.join(" "), sig.provides)
}

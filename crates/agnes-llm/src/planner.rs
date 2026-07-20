use crate::dsl_extract::extract_dsl;
use crate::error::PlannerError;
use crate::provider::{CompletionRequest, Message, Provider, Role};
use agnes_registry::Registry;
use agnes_types::ToolSignature;
use std::sync::Arc;

const MAX_TURNS_VERBATIM: usize = 6;
const PLANNER_MAX_TOKENS: u32 = 2048;

#[derive(Debug, Clone)]
pub struct Turn {
    pub user_nl: String,
    pub assistant_dsl: String,
    pub result_preview: String,
}

/// Draft-buffer entry: raw messages appended during this in-flight turn.
#[derive(Debug, Clone)]
enum Scratch {
    User(String),
    Assistant(String),
}

pub struct Planner {
    provider: Arc<dyn Provider>,
    system: String,
    history: Vec<Turn>,
    /// Uncommitted messages for the turn currently being planned.
    scratch: Vec<Scratch>,
    /// The natural-language prompt that started the current in-flight turn.
    pending_nl: Option<String>,
}

impl Planner {
    pub fn new(provider: Arc<dyn Provider>, registry: &Registry) -> Self {
        Self {
            provider,
            system: build_system_prompt(registry),
            history: Vec::new(),
            scratch: Vec::new(),
            pending_nl: None,
        }
    }

    pub fn history(&self) -> &[Turn] {
        &self.history
    }

    pub fn reset_history(&mut self) {
        self.history.clear();
        self.scratch.clear();
        self.pending_nl = None;
    }

    /// Plan the DSL for `nl`. Call again after `push_error_feedback` to
    /// retry with the previous bad DSL and error in the message chain.
    pub async fn plan(&mut self, nl: &str) -> Result<String, PlannerError> {
        if self.pending_nl.is_none() {
            self.pending_nl = Some(nl.to_string());
            self.scratch.push(Scratch::User(nl.to_string()));
        } else if !nl.is_empty() && self.pending_nl.as_deref() != Some(nl) {
            // Same turn, different NL text should not usually happen — treat as
            // a fresh user turn appended to the scratch.
            self.scratch.push(Scratch::User(nl.to_string()));
        }

        let req = CompletionRequest {
            system: Some(self.effective_system()),
            messages: self.build_messages(),
            max_tokens: PLANNER_MAX_TOKENS,
        };
        let raw = self.provider.complete(req).await?;
        let dsl = extract_dsl(&raw);
        if dsl.is_empty() {
            return Err(PlannerError::EmptyResponse);
        }
        self.scratch.push(Scratch::Assistant(dsl.clone()));
        Ok(dsl)
    }

    pub fn push_error_feedback(&mut self, bad_dsl: String, err: String) {
        // After a successful `plan()` the scratch tail is an assistant DSL
        // entry. Replace it with `bad_dsl` (rather than appending a fresh
        // assistant turn) so the message chain does not end up with two
        // consecutive assistant turns — some Messages APIs (Anthropic) 400
        // on that shape. If the tail is not an assistant entry (defensive),
        // fall back to appending.
        match self.scratch.last_mut() {
            Some(Scratch::Assistant(slot)) => *slot = bad_dsl,
            _ => self.scratch.push(Scratch::Assistant(bad_dsl)),
        }
        self.scratch.push(Scratch::User(format!(
            "That failed with: {err}\n\nFix and try again; output only the corrected DSL inside a ```agnes fenced block."
        )));
    }

    /// Abandon the current in-flight turn. Clears `scratch` and `pending_nl`
    /// without touching committed `history`. Call this when a caller has
    /// decided the current turn cannot recover.
    pub fn abandon_pending_turn(&mut self) {
        self.scratch.clear();
        self.pending_nl = None;
    }

    pub fn record_result(&mut self, dsl: String, result_preview: String) {
        let user_nl = self.pending_nl.take().unwrap_or_default();
        self.history.push(Turn {
            user_nl,
            assistant_dsl: dsl,
            result_preview,
        });
        self.scratch.clear();
    }

    fn effective_system(&self) -> String {
        // Collapse anything beyond the last MAX_TURNS_VERBATIM into a
        // prefix line prepended to the system prompt.
        let n = self.history.len();
        if n <= MAX_TURNS_VERBATIM {
            return self.system.clone();
        }
        let extras: &[Turn] = &self.history[..n - MAX_TURNS_VERBATIM];
        let mut prefix = String::from("<prior context:\n");
        for t in extras {
            prefix.push_str(&format!(
                "  - user asked {:?}, produced {}-line DSL, result was {} chars\n",
                t.user_nl,
                t.assistant_dsl.lines().count(),
                t.result_preview.chars().count(),
            ));
        }
        prefix.push_str(">\n\n");
        prefix.push_str(&self.system);
        prefix
    }

    fn build_messages(&self) -> Vec<Message> {
        let mut out = Vec::new();
        // Verbatim slice of recent history.
        let n = self.history.len();
        let start = n.saturating_sub(MAX_TURNS_VERBATIM);
        for t in &self.history[start..] {
            out.push(Message {
                role: Role::User,
                content: t.user_nl.clone(),
            });
            out.push(Message {
                role: Role::Assistant,
                content: format!("```agnes\n{}\n```", t.assistant_dsl),
            });
        }
        // Then the scratch buffer for the in-flight turn.
        for s in &self.scratch {
            match s {
                Scratch::User(c) => out.push(Message {
                    role: Role::User,
                    content: c.clone(),
                }),
                Scratch::Assistant(c) => out.push(Message {
                    role: Role::Assistant,
                    content: format!("```agnes\n{c}\n```"),
                }),
            }
        }
        out
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

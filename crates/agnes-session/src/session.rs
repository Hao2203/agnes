use crate::error::SessionError;
use crate::events::{EventSink, SessionEvent};
use crate::plan_tree::build_plan_tree;
use crate::tracer_bridge::{ChannelTracer, drain};
use agnes_builtins::{ToolImpl, native_dispatch, register_builtins, PathResolver};
use agnes_llm::{Planner, Provider, Turn};
use agnes_registry::Registry;
use agnes_runtime::execute_with;
use agnes_types::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Which "root shape" a Value carries — the classification used by the
/// agent loop to decide whether to terminate (Finish/Other) or feed
/// back to the planner (Observation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    Finish,
    Observation,
    Other,
}

/// Read `value.declared_type`'s outermost head; classify accordingly.
pub fn classify_root(value: &agnes_types::Value) -> RootKind {
    use agnes_types::TypeExpr;
    match &value.declared_type {
        TypeExpr::App { head, args } if args.len() == 1 => match head.0.as_str() {
            "Finish" => RootKind::Finish,
            "Observation" => RootKind::Observation,
            _ => RootKind::Other,
        },
        _ => RootKind::Other,
    }
}

/// For a Finish/Observation wrapper type, return the outermost name of
/// the inner type (for use as the `type="..."` attribute in observation
/// XML). Returns `None` for non-wrapper types.
pub fn extract_inner_type(t: &agnes_types::TypeExpr) -> Option<agnes_types::TypeName> {
    use agnes_types::TypeExpr;
    match t {
        TypeExpr::App { head, args } if args.len() == 1 => match head.0.as_str() {
            "Finish" | "Observation" => Some(match &args[0] {
                TypeExpr::Named(n) => n.clone(),
                TypeExpr::App {
                    head: inner_head, ..
                } => inner_head.clone(),
            }),
            _ => None,
        },
        _ => None,
    }
}

pub enum TurnInput {
    NaturalLanguage(String),
    RawDsl(String),
}

/// Default upper bound on iterations per user turn. Rationale: Claude
/// Code / LangGraph plan-and-execute defaults are 20-25. Each iteration
/// can hold a full pipe/par expression so the effective tool-call
/// budget is much higher.
pub const DEFAULT_MAX_TURNS: u32 = 20;

/// Observation text longer than this is truncated (middle-cut) before
/// being fed back to the planner. Rationale: 2000-4000 tokens depending
/// on language, matching Anthropic's tool_result guideline.
pub const OBSERVATION_TRUNCATION_THRESHOLD: usize = 8000;

pub struct Session {
    dispatch: HashMap<String, ToolImpl>,
    planner: Planner,
    max_turns: u32,
    /// Allowed root directory for file operations.
    /// If None, defaults to current working directory.
    allow_root: Option<PathBuf>,
    /// Whether shell execution is permitted.
    allow_shell: bool,
    /// Current event sink for the active turn, if available.
    /// We need an unsafe impl because the raw pointer doesn't auto-implement Send/Sync,
    /// but this is safe because we only access it during the turn when the pointer
    /// is guaranteed to be valid, and no concurrent access happens.
    current_sink: Option<*mut dyn EventSink>,
}

// Safety: The current_sink pointer is only set during an active turn
// and cleared before the reference becomes invalid. The Session is never
// accessed concurrently from multiple threads during execution, so this
// unsafe impl is correct - it just tells the compiler the Session is OK
// to be Send/Sync which it is in practice.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("dispatch", &format_args!("HashMap<String, ToolImpl>"))
            .field("planner", &format_args!("Planner"))
            .field("max_turns", &self.max_turns)
            .field("allow_root", &self.allow_root)
            .field("allow_shell", &self.allow_shell)
            .finish()
    }
}

/// Helper to show a value using the builtins registry
fn show_value(value: &Value) -> String {
    let mut reg = Registry::new();
    register_builtins(&mut reg).ok();
    reg.show_value(value)
}

impl Session {
    pub fn new(provider: Arc<dyn Provider>) -> Result<Self, SessionError> {
        Self::new_with_max_turns(provider, DEFAULT_MAX_TURNS)
    }

    pub fn new_with_max_turns(
        provider: Arc<dyn Provider>,
        max_turns: u32,
    ) -> Result<Self, SessionError> {
        let mut registry = Registry::new();
        register_builtins(&mut registry).map_err(|e| SessionError::Check(e.to_string()))?;
        let dispatch = native_dispatch(provider.clone());
        let planner = Planner::new(provider, &registry);
        Ok(Self {
            dispatch,
            planner,
            max_turns,
            allow_root: None,
            allow_shell: false,
            current_sink: None,
        })
    }

    /// Builder method to set allowed root directory.
    pub fn with_allow_root(mut self, path: PathBuf) -> Self {
        let canonical = std::fs::canonicalize(&path)
            .unwrap_or_else(|e| panic!("failed to canonicalize allow_root path '{}': {}", path.display(), e));
        self.allow_root = Some(canonical);
        self
    }

    /// Builder method to enable shell execution.
    pub fn with_allow_shell(mut self, enabled: bool) -> Self {
        self.allow_shell = enabled;
        self
    }

    /// Get whether shell execution is permitted.
    pub fn allow_shell(&self) -> bool {
        self.allow_shell
    }

    /// Emit a session event to the registered event sink.
    /// Must only be called during an active turn.
    pub async fn emit_event(&self, event: SessionEvent) {
        if let Some(sink_ptr) = self.current_sink {
            // Safety: this is safe because:
            // 1. The pointer is only set during an active turn
            // 2. The mutable reference is guaranteed to be valid for the entire turn
            // 3. We don't alias the mutable reference in an unsafe way
            unsafe { &mut *sink_ptr }.emit(event).await;
        }
    }

    /// Resolve and validate a user-provided path against the allowed root.
    pub async fn resolve_path(&self, input: &str) -> Result<PathBuf, String> {
        let current_dir = std::env::current_dir()
            .map_err(|e| format!("failed to get current directory: {}", e))?;

        let allow_root = self.allow_root.as_ref()
            .unwrap_or(&current_dir);

        // Resolve input path against current working directory
        let candidate = if std::path::Path::new(input).is_absolute() {
            PathBuf::from(input)
        } else {
            current_dir.join(input)
        };

        // Canonicalize to resolve symlinks and .. components
        let canonical = tokio::fs::canonicalize(&candidate)
            .await
            .map_err(|e| format!("cannot resolve path '{}': {}", input, e))?;

        // Check that the canonical path starts with the allowed root
        if !canonical.starts_with(allow_root) {
            return Err(format!(
                "path '{}' (resolved to '{}') is outside allowed root directory '{}'",
                input, canonical.display(), allow_root.display()
            ));
        }

        Ok(canonical)
    }

    pub fn history(&self) -> &[Turn] {
        self.planner.history()
    }

    pub fn reset_history(&mut self) {
        self.planner.reset_history();
    }

    /// Drain the process-global write-file recorder. Called at the end of
    /// every turn (success and failure) so that writes never leak across
    /// turns and the sink gets a single `WriteSummary` per turn.
    fn drain_writes() -> Vec<(String, usize)> {
        let mut w = agnes_builtins::writes()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *w)
    }

    /// Truncate an observation string to `OBSERVATION_TRUNCATION_THRESHOLD`
    /// characters using a middle-cut, keeping the first and last quarters.
    fn truncate_observation(text: String) -> String {
        if text.chars().count() <= OBSERVATION_TRUNCATION_THRESHOLD {
            return text;
        }
        let total_chars = text.chars().count();
        let keep = OBSERVATION_TRUNCATION_THRESHOLD / 2;
        let first: String = text.chars().take(keep).collect();
        let last: String = text
            .chars()
            .rev()
            .take(keep)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let dropped = total_chars - 2 * keep;
        format!(
            "{first}\n\n... [truncated {dropped} chars — full length: {total_chars}] ...\n\n{last}"
        )
    }

    pub async fn run_turn(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
    ) -> Result<Value, SessionError> {
        // Uncancellable variant: a fresh Notify never fires.
        let never = Arc::new(tokio::sync::Notify::new());
        self.run_turn_cancellable(input, sink, never).await
    }

    pub async fn run_turn_cancellable(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
        cancel: Arc<tokio::sync::Notify>,
    ) -> Result<Value, SessionError> {
        // Store the current sink for event emission during tool execution
        // This is safe because:
        // 1. We clear the pointer before the reference becomes invalid
        // 2. We only dereference it during the turn when the reference is valid
        // 3. The unsafe impl just makes the compiler happy about Send/Sync
        use std::mem;
        self.current_sink = Some(unsafe { mem::transmute::<*mut dyn EventSink, *mut (dyn EventSink + 'static)>(sink) });
        let result = match self.run_turn_inner(input, sink, cancel).await {
            Ok(v) => Ok(v),
            Err(e) => {
                let recorded = Self::drain_writes();
                if !recorded.is_empty() {
                    sink.emit(SessionEvent::WriteSummary { entries: recorded })
                        .await;
                }
                sink.emit(SessionEvent::TurnFailed {
                    error: e.to_string(),
                })
                .await;
                Err(e)
            }
        };
        // Clear the current sink after the turn completes
        self.current_sink = None;
        result
    }

    async fn run_turn_inner(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
        cancel: Arc<tokio::sync::Notify>,
    ) -> Result<Value, SessionError> {
        // Seed: NL starts an in-flight planner turn; RawDsl provides
        // iter=0's DSL directly and still opens a planner turn (so
        // history is coherent for future turns).
        let (user_nl, mut seeded_dsl) = match input {
            TurnInput::NaturalLanguage(nl) => (nl, None),
            TurnInput::RawDsl(s) => (format!("/run {s}"), Some(s)),
        };
        self.planner.begin_user_turn(user_nl);

        let mut result = None;
        let mut iter = 0;
        while iter < self.max_turns && result.is_none() {
            // Cancellation check BEFORE emitting IterationStart, so a
            // pre-fired cancel returns with after_iterations = iter (0).
            if cancel_fired(&cancel) {
                self.planner.abandon_pending_turn();
                return Err(SessionError::Cancelled {
                    after_iterations: iter,
                });
            }
            sink.emit(SessionEvent::IterationStart { iter }).await;

            // Get the DSL for this iteration: either the seeded RawDsl
            // (iter 0 only) or a fresh planner call.
            let dsl = match seeded_dsl.take() {
                Some(s) => {
                    // We didn't go through plan_next, but the Planner still
                    // needs to know about this assistant turn. Feed it in
                    // synthetically: append an iteration whose assistant_dsl
                    // is the raw source. push_observation / record_finish
                    // in the branches below will operate on this iteration.
                    self.planner.inject_assistant_dsl(s.clone());
                    s
                }
                None => {
                    sink.emit(SessionEvent::PlannerStart).await;
                    self.planner.plan_next().await?
                }
            };
            sink.emit(SessionEvent::DslProduced {
                source: dsl.clone(),
            })
            .await;

            // Try to execute this iteration.
            let try_result = self.try_execute(&dsl, sink).await;

            match try_result {
                Ok(value) => {
                    match classify_root(&value) {
                        RootKind::Observation => {
                            let inner_type = extract_inner_type(&value.declared_type);
                            let raw = show_value(&value);
                            let text = Self::truncate_observation(raw);
                            sink.emit(SessionEvent::ObservationEmitted {
                                iter,
                                text: text.clone(),
                                is_error: false,
                            })
                            .await;
                            self.planner
                                .push_observation(dsl.clone(), text, false, inner_type);
                            // Loop continues to next iteration
                            iter += 1;
                        }
                        RootKind::Finish | RootKind::Other => {
                            let s = show_value(&value);
                            let recorded = Self::drain_writes();
                            if !recorded.is_empty() {
                                sink.emit(SessionEvent::WriteSummary { entries: recorded })
                                    .await;
                            }
                            sink.emit(SessionEvent::TurnResult {
                                value_preview: s.clone(),
                                value_type: value.declared_type.to_string(),
                            })
                            .await;
                            self.planner.record_finish(dsl, s);
                            result = Some(Ok(value));
                            // Loop terminates immediately
                            break;
                        }
                    }
                }
                Err(e) => {
                    let text = e.to_string();
                    sink.emit(SessionEvent::ObservationEmitted {
                        iter,
                        text: text.clone(),
                        is_error: true,
                    })
                    .await;
                    self.planner.push_observation(dsl, text, true, None);
                    // Loop continues; do NOT drain writes here — a failed
                    // iteration should not leak writes into the next.
                    let _ = Self::drain_writes();
                    iter += 1;
                }
            }
        }

        if let Some(result) = result {
            result
        } else {
            // Loop fell through — MAX_TURNS reached.
            self.planner.abandon_pending_turn();
            Err(SessionError::TurnLimitExceeded {
                max_turns: self.max_turns,
            })
        }
    }

    /// One iteration: parse/check/compile/execute a DSL. Emits DslProduced
    /// (already emitted by caller), PlanReady, NodeStart/NodeEnd via tracer.
    async fn try_execute(
        &mut self,
        dsl: &str,
        sink: &mut dyn EventSink,
    ) -> Result<Value, SessionError> {
        let program = agnes_parser::parse(dsl).map_err(|e| SessionError::Parse(e.to_string()))?;
        let mut turn_registry = Registry::new();
        register_builtins(&mut turn_registry).map_err(|e| SessionError::Check(e.to_string()))?;
        turn_registry
            .load(&program)
            .map_err(|e| SessionError::Check(e.to_string()))?;
        agnes_checker::check(&program, &turn_registry)
            .map_err(|e| SessionError::Check(e.to_string()))?;
        let dag = agnes_compiler::compile(&program, &turn_registry)
            .map_err(|e| SessionError::Compile(e.to_string()))?;
        sink.emit(SessionEvent::PlanReady {
            tree: build_plan_tree(&dag),
        })
        .await;
        let (tracer, mut rx) = ChannelTracer::new();
        let exec = execute_with(&dag, &turn_registry, &self.dispatch, self, &tracer);
        tokio::pin!(exec);
        let result = loop {
            tokio::select! {
                r = &mut exec => break r,
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    drain(&mut rx, sink).await;
                }
            }
        };
        drain(&mut rx, sink).await;
        Ok(result?)
    }
}

impl PathResolver for Session {
    fn resolve_path<'a>(&'a self, input: &'a str) -> agnes_builtins::BoxFuture<'a, Result<std::path::PathBuf, String>> {
        Box::pin(self.resolve_path(input))
    }

    fn allow_shell(&self) -> bool {
        self.allow_shell
    }

    fn emit_shell_confirm<'a>(
        &'a self,
        command: String,
        responder: tokio::sync::oneshot::Sender<bool>,
    ) -> agnes_builtins::BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(sink_ptr) = self.current_sink {
                // Safety: this is safe because:
                // 1. The pointer is only stored during an active turn
                // 2. The original mutable reference is guaranteed to be valid for the entire turn
                // 3. We clear the pointer before the reference becomes invalid
                // 4. Execution is sequential so there's no concurrent mutable access
                unsafe { &mut *sink_ptr }.emit(SessionEvent::ShellConfirm {
                    command,
                    responder: std::sync::Arc::new(responder),
                }).await;
            } else {
                // No sink available, automatically reject
                let _ = responder.send(false);
            }
        })
    }
}

/// Non-async, non-blocking check: has the Notify been signaled? We
/// implement this via a try_recv-shaped pattern using `try_notified`.
/// tokio::sync::Notify doesn't have a direct "is signaled" query, but
/// a fresh Notified future polled once returns Ready if a permit is
/// stored.
fn cancel_fired(n: &tokio::sync::Notify) -> bool {
    // If notify_one was called, one permit is stored; a fresh notified()
    // future returns Ready(()) on first poll.
    let mut fut = std::pin::pin!(n.notified());
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    matches!(fut.as_mut().poll(&mut cx), Poll::Ready(()))
}

use crate::error::SessionError;
use crate::events::{EventSink, SessionEvent};
use crate::plan_tree::build_plan_tree;
use crate::tracer_bridge::{ChannelTracer, drain};
use agnes_builtins::{ToolImpl, native_dispatch, register_builtins};
use agnes_llm::{Planner, Provider, Turn};
use agnes_registry::Registry;
use agnes_runtime::execute_with;
use agnes_types::{TypeExpr, TypeName, Value};
use std::collections::HashMap;
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
                TypeExpr::App { head: inner_head, .. } => inner_head.clone(),
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

pub struct Session {
    dispatch: HashMap<String, ToolImpl>,
    planner: Planner,
}

const MAX_PLAN_RETRIES: u8 = 2;

impl Session {
    pub fn new(provider: Arc<dyn Provider>) -> Result<Self, SessionError> {
        // A template registry is used once, only to seed the planner's system
        // prompt (Planner captures what it needs by reference at construction
        // time). Each `run_turn` builds its own fresh per-turn Registry, so
        // we deliberately drop this one at the end of `new`.
        let mut registry = Registry::new();
        register_builtins(&mut registry).map_err(|e| SessionError::Check(e.to_string()))?;
        let dispatch = native_dispatch(provider.clone());
        let planner = Planner::new(provider, &registry);
        Ok(Self { dispatch, planner })
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

    pub async fn run_turn(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
    ) -> Result<Value, SessionError> {
        match self.run_turn_inner(input, sink).await {
            Ok(v) => Ok(v),
            Err(e) => {
                // Drain any pending write-file records so they don't leak
                // into the next turn, and surface them alongside the
                // failure event.
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
        }
    }

    async fn run_turn_inner(
        &mut self,
        input: TurnInput,
        sink: &mut dyn EventSink,
    ) -> Result<Value, SessionError> {
        let dsl = match input {
            TurnInput::RawDsl(s) => s,
            TurnInput::NaturalLanguage(nl) => {
                sink.emit(SessionEvent::PlannerStart).await;
                self.plan_with_retries(&nl, sink).await?
            }
        };
        sink.emit(SessionEvent::DslProduced {
            source: dsl.clone(),
        })
        .await;

        // parse -> check -> compile
        let program = agnes_parser::parse(&dsl).map_err(|e| SessionError::Parse(e.to_string()))?;
        // A registry mutation-per-turn: apply top-levels of this program.
        // For the MVP we do NOT persist `define`s across turns — each turn
        // gets a fresh registry seeded with the builtins. This trades the
        // "prior-turn defines visible to later turns" nicety for a much
        // simpler correctness story (no duplicate-define NameConflict when
        // the same DSL is re-run, no state that surprises the user).
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
        // Poll the channel while the runtime executes.
        let exec = execute_with(&dag, &turn_registry, &self.dispatch, &tracer);
        tokio::pin!(exec);
        let result = loop {
            tokio::select! {
                r = &mut exec => break r,
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    drain(&mut rx, sink).await;
                }
            }
        };
        // Final drain — pick up any events emitted after the last tick.
        drain(&mut rx, sink).await;

        let v = result?;
        let preview = if let Some(s) = v.data.as_str() {
            let t: String = s.chars().take(120).collect();
            format!("{t}{}", if s.len() > 120 { "…" } else { "" })
        } else {
            v.data.to_string()
        };
        // Drain the per-turn write-file recorder before the closing event
        // so the sink sees `WriteSummary` immediately before `TurnResult`.
        let recorded = Self::drain_writes();
        if !recorded.is_empty() {
            sink.emit(SessionEvent::WriteSummary { entries: recorded })
                .await;
        }
        sink.emit(SessionEvent::TurnResult {
            value_preview: preview.clone(),
            value_type: v.declared_type.to_string(),
        })
        .await;
        self.planner.record_result(dsl, preview);
        Ok(v)
    }

    async fn plan_with_retries(
        &mut self,
        nl: &str,
        sink: &mut dyn EventSink,
    ) -> Result<String, SessionError> {
        let mut last_err = String::new();
        for attempt in 0..=MAX_PLAN_RETRIES {
            let dsl = self.planner.plan(nl).await?;
            // Dry-run: parse/check/compile against a fresh registry so a
            // planner attempt with a bad `define` cannot break anything.
            let mut probe = Registry::new();
            register_builtins(&mut probe).map_err(|e| SessionError::Check(e.to_string()))?;
            match dry_run(&dsl, &mut probe) {
                Ok(()) => return Ok(dsl),
                Err(e) => {
                    last_err = e.clone();
                    if attempt < MAX_PLAN_RETRIES {
                        sink.emit(SessionEvent::PlannerRetry {
                            attempt: attempt + 1,
                            error: e.clone(),
                        })
                        .await;
                        self.planner.push_error_feedback(dsl, e);
                    }
                }
            }
        }
        // The in-flight turn is unrecoverable — reset the planner's scratch
        // buffer and pending NL so the next `plan()` call starts a fresh
        // turn with a User-role-first message chain (F1 regression guard).
        // Committed `history` is preserved.
        self.planner.abandon_pending_turn();
        Err(SessionError::TurnLimitExceeded { max_turns: (MAX_PLAN_RETRIES + 1) as u32 })
    }
}

fn dry_run(dsl: &str, probe: &mut Registry) -> Result<(), String> {
    let program = agnes_parser::parse(dsl).map_err(|e| e.to_string())?;
    probe.load(&program).map_err(|e| e.to_string())?;
    agnes_checker::check(&program, probe).map_err(|e| e.to_string())?;
    let _ = agnes_compiler::compile(&program, probe).map_err(|e| e.to_string())?;
    Ok(())
}

use crate::plan_tree::PlanTree;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

#[derive(Debug, Clone)]
pub enum NodeKindTag {
    Tool { name: String },
    Llm,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SessionEvent {
    PlannerStart,
    PlannerRetry {
        attempt: u8,
        error: String,
    },
    DslProduced {
        source: String,
    },
    PlanReady {
        tree: PlanTree,
    },
    NodeStart {
        id: u32,
        kind: NodeKindTag,
        args: Vec<(String, String)>,
    },
    NodeEnd {
        id: u32,
        ok: bool,
        preview: String,
        elapsed_ms: u64,
    },
    TurnResult {
        value_preview: String,
        value_type: String,
    },
    TurnFailed {
        error: String,
    },
    /// `write-file` invocations that occurred during this turn.
    /// (path, byte-count) pairs, in call order.
    WriteSummary {
        entries: Vec<(String, usize)>,
    },

    /// Emitted at the start of each planner↔runtime iteration in a turn.
    /// `iter` is 0-indexed.
    IterationStart {
        iter: u32,
    },

    /// Emitted when the current iteration's result is fed back to the
    /// planner as an observation (i.e. runtime returned Observation _
    /// or errored). `is_error=true` means the runtime threw a
    /// parse/check/compile/execute error rather than emitting a value.
    ObservationEmitted {
        iter: u32,
        text: String,
        is_error: bool,
    },

    /// Request user confirmation before executing a shell command.
    ShellConfirm {
        /// The command to execute.
        command: String,
        /// Send `true` to approve, `false` to cancel.
        responder: Arc<oneshot::Sender<bool>>,
    },
}

#[async_trait::async_trait]
pub trait EventSink: Send {
    async fn emit(&mut self, ev: SessionEvent);
}

/// A borrowed handle to the turn's event sink that acquires the shared
/// `Mutex` on each `emit`, instead of holding a single lock guard across
/// the whole turn.
///
/// Holding one `MutexGuard` for an entire turn deadlocks any tool that
/// re-enters the sink through `PathResolver::emit_shell_confirm`
/// (notably `shell-run`): the tool awaits the same mutex the turn task
/// is already holding, and the turn task is itself awaiting the tool's
/// result. `SinkHandle` breaks the cycle by locking per event and
/// releasing between emits, so the tool can acquire the sink while the
/// turn is parked on `execute_with`.
#[derive(Clone, Copy)]
pub struct SinkHandle<'a> {
    sink: &'a Arc<Mutex<dyn EventSink + Send + 'static>>,
}

impl<'a> SinkHandle<'a> {
    pub fn new(sink: &'a Arc<Mutex<dyn EventSink + Send + 'static>>) -> Self {
        Self { sink }
    }

    /// Lock the shared sink for a single emit, then release. Safe for a
    /// tool running under the same turn to call concurrently.
    pub async fn emit(&self, ev: SessionEvent) {
        self.sink.lock().await.emit(ev).await;
    }
}

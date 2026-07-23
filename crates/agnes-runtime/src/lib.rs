//! agnes runtime: tokio async executor with boundary validation.

pub mod boundary;
pub mod error;
mod scheduler;

pub use error::RuntimeError;

use std::collections::HashMap;
use std::time::Duration;

use agnes_builtins::{ToolCtx, ToolImpl};
use agnes_compiler::{Dag, NodeId, NodeKind};
use agnes_registry::Registry;
use agnes_types::Value;

/// Observer for tool node execution. Hooks fire only for
/// `NodeKind::Tool { .. }` — control-flow nodes are
/// silent. `args_preview` is a caller-formatted, truncation-friendly
/// summary of the arg map (no exact contract; consumers must tolerate any
/// human string).
pub trait Tracer: Send + Sync {
    fn node_start(&self, id: NodeId, kind: &NodeKind, args_preview: &str);
    fn node_end(&self, id: NodeId, result: Result<&Value, &RuntimeError>, elapsed: Duration);
}

/// Default no-op tracer used by the plain `execute()` entry point.
pub struct NoopTracer;

impl Tracer for NoopTracer {
    fn node_start(&self, _id: NodeId, _kind: &NodeKind, _args_preview: &str) {}
    fn node_end(&self, _id: NodeId, _result: Result<&Value, &RuntimeError>, _elapsed: Duration) {}
}

pub async fn execute(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    ctx: &ToolCtx<'_>,
) -> Result<Value, RuntimeError> {
    execute_with(dag, reg, dispatch, ctx, &NoopTracer).await
}

pub async fn execute_with(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    ctx: &ToolCtx<'_>,
    tracer: &dyn Tracer,
) -> Result<Value, RuntimeError> {
    scheduler::run(dag, reg, dispatch, ctx, tracer).await
}

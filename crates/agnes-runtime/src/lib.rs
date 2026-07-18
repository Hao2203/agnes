//! agnes runtime: tokio async executor with boundary validation.

pub mod boundary;
pub mod error;
mod scheduler;

pub use error::RuntimeError;

use std::collections::HashMap;

use agnes_builtins::ToolImpl;
use agnes_compiler::Dag;
use agnes_registry::Registry;
use agnes_types::Value;

pub async fn execute(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
) -> Result<Value, RuntimeError> {
    scheduler::run(dag, reg, dispatch).await
}

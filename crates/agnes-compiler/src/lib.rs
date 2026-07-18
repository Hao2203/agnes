//! AST -> DAG compilation, including recursive-define detection.

mod cycle;
pub mod dag;
mod lower;

pub use dag::{Dag, Input, Node, NodeId, NodeKind};

use agnes_ast::Program;
use agnes_registry::Registry;

#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error(
        "Recursive define detected: `{name}` calls itself (directly or transitively).\n  MVP does not support recursion; refactor the workflow to a fixed-depth chain."
    )]
    CycleDetected { name: String },

    #[error(transparent)]
    Registry(#[from] agnes_registry::RegistryError),

    #[error("Compilation failure: {name}")]
    UnknownDefine { name: String },
}

pub fn compile(program: &Program, registry: &Registry) -> Result<Dag, CompileError> {
    if let Some(name) = cycle::detect_define_cycles(program) {
        return Err(CompileError::CycleDetected { name });
    }
    let mut lower = lower::Lowering::new(registry);
    lower.lower_program(program)
}

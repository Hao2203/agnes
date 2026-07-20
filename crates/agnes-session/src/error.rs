use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Planner(#[from] agnes_llm::PlannerError),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("check error: {0}")]
    Check(String),

    #[error("compile error: {0}")]
    Compile(String),

    #[error(transparent)]
    Runtime(#[from] agnes_runtime::RuntimeError),

    #[error("planner exhausted retries (attempts=3); last error: {last}")]
    RetriesExhausted { last: String },
}

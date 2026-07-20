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

    #[error(
        "Agent loop hit the iteration limit.\n  Why: `MAX_TURNS = {max_turns}` reached without a terminating iteration (finish or unlabeled result).\n  Fix: rephrase the request more narrowly, or pass `--max-turns <N>` to raise the ceiling."
    )]
    TurnLimitExceeded { max_turns: u32 },
}

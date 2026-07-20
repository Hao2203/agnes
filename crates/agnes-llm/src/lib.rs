//! LLM provider abstraction + Planner (NL -> agnes DSL).
//!
//! Two production providers live in `anthropic` and `openai` (added in
//! Tasks 2 and 3). `MockProvider` is available at all times for tests.

mod anthropic;
mod dsl_extract;
mod error;
mod mock;
mod openai;
mod planner;
mod provider;
mod resolve;

pub use anthropic::AnthropicProvider;
pub use dsl_extract::extract_dsl;
pub use error::{LlmError, PlannerError};
pub use mock::MockProvider;
pub use openai::OpenAiCompatProvider;
pub use planner::{Iteration, Observation, Planner, Turn, TurnOutcome};
pub use provider::{CompletionRequest, Message, Provider, Role};
pub use resolve::{LlmCliOpts, ResolvedKind, resolve_decision, resolve_provider};

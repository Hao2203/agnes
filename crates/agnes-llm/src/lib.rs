//! LLM provider abstraction + Planner (NL -> agnes DSL).
//!
//! Two production providers live in `anthropic` and `openai` (added in
//! Tasks 2 and 3). `MockProvider` is available at all times for tests.

mod error;
mod mock;
mod provider;

pub use error::LlmError;
pub use mock::MockProvider;
pub use provider::{CompletionRequest, Message, Provider, Role};

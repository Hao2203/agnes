//! Pick a `Provider` from CLI flags, falling back to env vars, then defaults.
//!
//! `.env` loading is the caller's responsibility (the CLI does it once at
//! startup via `dotenvy::dotenv().ok()` before calling `resolve_provider`).

use crate::anthropic::AnthropicProvider;
use crate::error::LlmError;
use crate::openai::OpenAiCompatProvider;
use crate::provider::Provider;
use std::sync::Arc;

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";

#[derive(Debug, Clone, Default)]
pub struct LlmCliOpts {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
}

/// Which provider kind will be constructed for the given options.
/// Exposed for testability and diagnostics; the returned decision is what
/// `resolve_provider` uses internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedKind {
    Anthropic,
    OpenAiCompat,
}

fn pick(flag: &Option<String>, env: &str) -> Option<String> {
    flag.clone()
        .or_else(|| std::env::var(env).ok().filter(|s| !s.is_empty()))
}

/// Decide the (kind, model) that `resolve_provider` would build, without
/// touching the network or constructing a client. Returns the same error
/// values `resolve_provider` returns for the same input.
pub fn resolve_decision(cli: &LlmCliOpts) -> Result<(ResolvedKind, String), LlmError> {
    let provider = pick(&cli.provider, "AGNES_LLM_PROVIDER").ok_or(LlmError::MissingConfig {
        what: "provider selection",
        env_var: "AGNES_LLM_PROVIDER",
        flag: "--llm-provider",
    })?;
    let model = pick(&cli.model, "AGNES_LLM_MODEL");

    match provider.as_str() {
        "anthropic" => {
            // Key check happens in `resolve_provider`; the decision itself
            // does not require a key. But to keep the error surface identical
            // for `resolve_decision`, we still surface missing key here so
            // callers see the same first-error the builder would.
            let _key = std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or(LlmError::MissingApiKey {
                    env_var: "ANTHROPIC_API_KEY",
                })?;
            let model = model.unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string());
            Ok((ResolvedKind::Anthropic, model))
        }
        "openai" => {
            let _key = std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or(LlmError::MissingApiKey {
                    env_var: "OPENAI_API_KEY",
                })?;
            let _base = pick(&cli.base_url, "AGNES_LLM_BASE_URL").ok_or(
                LlmError::MissingConfig {
                    what: "base_url",
                    env_var: "AGNES_LLM_BASE_URL",
                    flag: "--llm-base-url",
                },
            )?;
            let model = model.ok_or(LlmError::MissingConfig {
                what: "model",
                env_var: "AGNES_LLM_MODEL",
                flag: "--llm-model",
            })?;
            Ok((ResolvedKind::OpenAiCompat, model))
        }
        other => Err(LlmError::UnknownProvider {
            name: other.to_string(),
        }),
    }
}

pub fn resolve_provider(cli: &LlmCliOpts) -> Result<Arc<dyn Provider>, LlmError> {
    let (kind, model) = resolve_decision(cli)?;
    let client = reqwest::Client::new();
    match kind {
        ResolvedKind::Anthropic => {
            // Decision has already verified the key is present.
            let key = std::env::var("ANTHROPIC_API_KEY").expect("checked by resolve_decision");
            Ok(Arc::new(AnthropicProvider::new(model, key, client)))
        }
        ResolvedKind::OpenAiCompat => {
            let key = std::env::var("OPENAI_API_KEY").expect("checked by resolve_decision");
            let base = pick(&cli.base_url, "AGNES_LLM_BASE_URL")
                .expect("checked by resolve_decision");
            Ok(Arc::new(OpenAiCompatProvider::new(model, key, base, client)))
        }
    }
}

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("provider API returned status {status}\n  body: {body}\n  Fix: verify api key, model id, and base_url; retry.")]
    Api { status: u16, body: String },

    #[error("failed to deserialize provider response: {0}")]
    Deserialize(String),

    #[error("Missing API key.\n  Why: `{env_var}` is not set.\n  Fix: export {env_var}=<key>, or add `{env_var}=<key>` to a .env file at the workspace root.")]
    MissingApiKey { env_var: &'static str },

    #[error("Missing {what}.\n  Why: neither the CLI flag `{flag}` nor the env var `{env_var}` is set.\n  Fix: pass {flag}, set {env_var}, or add it to .env.")]
    MissingConfig {
        what: &'static str,
        env_var: &'static str,
        flag: &'static str,
    },

    #[error("Unknown provider `{name}`.\n  Fix: use one of: `anthropic`, `openai`.")]
    UnknownProvider { name: String },
}

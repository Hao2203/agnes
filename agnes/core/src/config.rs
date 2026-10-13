//! Boundary types: values that enter the system as free-form strings are
//! parsed exactly once and carried as newtypes from then on.

use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("base URL must start with `http://` or `https://`: {value}")]
    BadScheme { value: String },
    #[error("turn budget must be a positive integer: {value}")]
    BadTurnBudget { value: String },
    #[error("tool name must not contain whitespace: {value}")]
    ToolNameWhitespace { value: String },
}

/// Name of the model to request, e.g. `gpt-4o-mini`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelName(String);

impl ModelName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ModelName {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::Empty { field: "model" });
        }
        Ok(Self(trimmed.to_owned()))
    }
}

/// The system prompt of an agent run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPrompt(String);

impl SystemPrompt {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Secret credential. `Debug` never prints the value so keys cannot leak
/// through logs.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(value: impl Into<String>) -> Result<Self, ConfigError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ConfigError::Empty { field: "api key" });
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

impl FromStr for ApiKey {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Root of an OpenAI-compatible API, e.g. `https://api.openai.com/v1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl(String);

impl BaseUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Appends a path (must start with `/`) to the normalized base URL.
    pub fn join(&self, path: &str) -> String {
        debug_assert!(path.starts_with('/'));
        format!("{}{}", self.0, path)
    }
}

impl FromStr for BaseUrl {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let lower = trimmed.to_ascii_lowercase();
        let scheme_len = if lower.starts_with("https://") {
            8
        } else if lower.starts_with("http://") {
            7
        } else {
            return Err(ConfigError::BadScheme {
                value: trimmed.to_owned(),
            });
        };
        let host = trimmed[scheme_len..].trim_end_matches('/');
        if host.is_empty() {
            return Err(ConfigError::BadScheme {
                value: trimmed.to_owned(),
            });
        }
        let mut normalized = trimmed.to_owned();
        while normalized.ends_with('/') {
            normalized.pop();
        }
        Ok(Self(normalized))
    }
}

/// Upper bound on model turns; guarantees the agent loop terminates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnBudget(NonZeroU32);

impl TurnBudget {
    pub fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    pub fn max_turns(&self) -> u32 {
        self.0.get()
    }
}

impl FromStr for TurnBudget {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim()
            .parse::<NonZeroU32>()
            .map(Self)
            .map_err(|_| ConfigError::BadTurnBudget {
                value: s.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_name_trims_and_rejects_empty() {
        assert_eq!(
            "gpt-4o-mini".parse::<ModelName>().unwrap().as_str(),
            "gpt-4o-mini"
        );
        assert_eq!(
            "  gpt-4o-mini  ".parse::<ModelName>().unwrap().as_str(),
            "gpt-4o-mini"
        );
        assert!("   ".parse::<ModelName>().is_err());
    }

    #[test]
    fn api_key_never_leaks_through_debug() {
        let key = "sk-secret".parse::<ApiKey>().unwrap();
        assert_eq!(format!("{key:?}"), "ApiKey(***)");
        assert_eq!(key.expose(), "sk-secret");
        assert!("".parse::<ApiKey>().is_err());
    }

    #[test]
    fn base_url_is_normalized() {
        let url = " https://api.openai.com/v1/// ".parse::<BaseUrl>().unwrap();
        assert_eq!(url.as_str(), "https://api.openai.com/v1");
        assert_eq!(
            url.join("/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert!("api.openai.com/v1".parse::<BaseUrl>().is_err());
        assert!("https://".parse::<BaseUrl>().is_err());
    }

    #[test]
    fn turn_budget_rejects_zero_and_garbage() {
        assert_eq!("8".parse::<TurnBudget>().unwrap().max_turns(), 8);
        assert!("0".parse::<TurnBudget>().is_err());
        assert!("many".parse::<TurnBudget>().is_err());
    }
}

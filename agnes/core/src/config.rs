//! Boundary types: values that enter the system as free-form strings are
//! parsed exactly once and carried as newtypes from then on.

use secrecy::{ExposeSecret, SecretString};
use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::Arc;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("base URL must start with `http://` or `https://`: {value}")]
    BadScheme { value: String },
    #[error("invalid base URL: {value}")]
    BadUrl {
        value: String,
        #[source]
        source: url::ParseError,
    },
    #[error("turn budget must be a positive integer: {value}")]
    BadTurnBudget { value: String },
    #[error("tool name must not contain whitespace: {value}")]
    ToolNameWhitespace { value: String },
}

/// Name of the model to request, e.g. `gpt-4o-mini`. Cloned on every run
/// and turn; refcounted, so clones are cheap regardless of length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelName(Arc<str>);

impl ModelName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ModelName {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::Empty { field: "model" });
        }
        Ok(Self(Arc::from(trimmed)))
    }
}

/// The system prompt of an agent run. Cloned when sessions are continued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPrompt(Arc<str>);

impl SystemPrompt {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(Arc::from(value.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Secret credential. The key material lives in a `secrecy::SecretString`:
/// wiped from memory on drop, reachable only through [`ApiKey::expose`], and
/// never printed by `Debug` — keys cannot leak through logs.
pub struct ApiKey(SecretString);

impl ApiKey {
    pub fn new(value: impl Into<String>) -> Result<Self, ConfigError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ConfigError::Empty { field: "api key" });
        }
        Ok(Self(value.into()))
    }

    /// Borrows the key material — the only way out, never implicit.
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
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
///
/// Normalized once at parse time: an absolute `http(s)` URL whose path ends
/// with exactly one `/`, so relative endpoints append to the base path
/// instead of replacing its last segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl(Arc<Url>);

impl BaseUrl {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Resolves a path relative to the base: `join("chat/completions")`
    /// against `https://api.openai.com/v1/` gives
    /// `https://api.openai.com/v1/chat/completions`.
    pub fn join(&self, path: &str) -> Url {
        debug_assert!(
            !path.starts_with('/'),
            "path must be relative to the base URL"
        );
        self.0
            .join(path)
            .expect("a relative path resolves against an absolute base URL")
    }
}

impl FromStr for BaseUrl {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let mut url = Url::parse(trimmed).map_err(|source| ConfigError::BadUrl {
            value: trimmed.to_owned(),
            source,
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ConfigError::BadScheme {
                value: trimmed.to_owned(),
            });
        }
        // `Url::join` replaces the last path segment unless the path ends
        // with `/`; pin exactly one trailing slash so `join` appends.
        let path = url.path();
        if !path.ends_with('/') || path.ends_with("//") {
            url.set_path(&format!("{}/", path.trim_end_matches('/')));
        }
        Ok(Self(Arc::new(url)))
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
    fn api_key_moves_an_owned_string_into_the_secret() {
        let key = ApiKey::new(String::from("  sk-owned  ")).unwrap();
        assert_eq!(key.expose(), "  sk-owned  ");
        assert!(!format!("{key:?}").contains("sk-owned"));
    }

    #[test]
    fn base_url_is_normalized() {
        let url = " https://api.openai.com/v1/// ".parse::<BaseUrl>().unwrap();
        assert_eq!(url.as_str(), "https://api.openai.com/v1/");
        assert_eq!(
            url.join("chat/completions").as_str(),
            "https://api.openai.com/v1/chat/completions"
        );

        let err = "api.openai.com/v1".parse::<BaseUrl>().unwrap_err();
        assert!(matches!(err, ConfigError::BadUrl { .. }));
        assert!(std::error::Error::source(&err).is_some());
        assert!("https://".parse::<BaseUrl>().is_err());
        assert!("ftp://api.openai.com/v1".parse::<BaseUrl>().is_err());
    }

    #[test]
    fn base_url_without_a_path_still_appends() {
        let url = "https://api.openai.com".parse::<BaseUrl>().unwrap();
        assert_eq!(url.as_str(), "https://api.openai.com/");
        assert_eq!(
            url.join("v1/chat/completions").as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn turn_budget_rejects_zero_and_garbage() {
        assert_eq!("8".parse::<TurnBudget>().unwrap().max_turns(), 8);
        assert!("0".parse::<TurnBudget>().is_err());
        assert!("many".parse::<TurnBudget>().is_err());
    }
}

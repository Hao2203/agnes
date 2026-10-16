//! Boundary types: values that enter the system as free-form strings are
//! parsed exactly once and carried as newtypes from then on.
//!
//! Internal string representations stay internal: newtypes wrap their storage
//! (`ArcColdString` for strings that may be short, `Arc<str>` for ones that
//! certainly cannot be inlined) and the public surface only carries `&str`,
//! `String`, and these newtypes.

use cold_string::ArcColdString;
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
    #[error("step budget must be a positive integer: {value}")]
    BadStepBudget { value: String },
    #[error("tool name must not contain whitespace: {value}")]
    ToolNameWhitespace { value: String },
}

/// Name of the model to request, e.g. `gpt-4o-mini`. Cloned on every run
/// and turn; short names are stored inline and longer ones are refcounted.
#[derive(Clone, PartialEq, Eq)]
pub struct ModelName(ArcColdString);

impl ModelName {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ModelName").field(&self.as_str()).finish()
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
        Ok(Self(ArcColdString::new(trimmed)))
    }
}

/// The system prompt of an agent run. Cloned when sessions are continued;
/// short prompts are stored inline and longer ones are refcounted.
#[derive(Clone, PartialEq, Eq)]
pub struct SystemPrompt(ArcColdString);

impl SystemPrompt {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(ArcColdString::new(value.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SystemPrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SystemPrompt").field(&self.as_str()).finish()
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

/// 上限：一个 turn 最多启动多少个 step（模型往返）；保证 agent 循环终止。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepBudget(NonZeroU32);

impl StepBudget {
    pub fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    pub fn max_steps(&self) -> u32 {
        self.0.get()
    }

    /// 已经启动 `started` 个 step，还能启动下一个吗？
    pub fn allows(&self, started: u32) -> bool {
        started < self.0.get()
    }
}

impl FromStr for StepBudget {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim()
            .parse::<NonZeroU32>()
            .map(Self)
            .map_err(|_| ConfigError::BadStepBudget {
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
    fn step_budget_rejects_zero_and_garbage() {
        assert_eq!("8".parse::<StepBudget>().unwrap().max_steps(), 8);
        assert!("0".parse::<StepBudget>().is_err());
        assert!("many".parse::<StepBudget>().is_err());
    }

    #[test]
    fn step_budget_covers_the_steps_it_allows() {
        let budget = "3".parse::<StepBudget>().unwrap();
        assert!(budget.allows(0));
        assert!(budget.allows(1));
        assert!(budget.allows(2));
        assert!(!budget.allows(3));
    }
}

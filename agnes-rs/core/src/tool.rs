//! Tool vocabulary: names, specs, and errors. Pure data — the async
//! [`agnes_io::tool::Tool`] seam and registry live in `agnes-io`.
//!
//! Names may be short and are cloned on every spec snapshot, so they wrap
//! `ArcColdString` (inline when short, refcounted when long). Descriptions are
//! prose that can never be inline, so they stay on the standard `Arc<str>`.
//! Both representations are internal; the public surface speaks `&str`.

use crate::config::ConfigError;
use cold_string::ArcColdString;
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

/// Name of a tool. Non-empty, no whitespace. Used as a map key and cloned
/// whenever specs are snapshotted; short names are stored inline and longer
/// ones are refcounted.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolName(ArcColdString);

impl ToolName {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ToolName").field(&self.as_str()).finish()
    }
}

impl PartialOrd for ToolName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ToolName {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ToolName {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::Empty { field: "tool name" });
        }
        if trimmed.chars().any(char::is_whitespace) {
            return Err(ConfigError::ToolNameWhitespace {
                value: trimmed.to_owned(),
            });
        }
        Ok(Self(ArcColdString::new(trimmed)))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("tool parameter schema must be a JSON object")]
pub struct SchemaError;

/// JSON schema describing a tool's parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonSchema(serde_json::Value);

impl TryFrom<serde_json::Value> for JsonSchema {
    type Error = SchemaError;

    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        if value.is_object() {
            Ok(Self(value))
        } else {
            Err(SchemaError)
        }
    }
}

impl JsonSchema {
    pub fn get(&self) -> &serde_json::Value {
        &self.0
    }
}

/// The contract a model sees: name, description, parameter schema. Cloned
/// whenever specs are snapshotted for a request. The description is prose
/// that can never be inline, so it stays on the standard `Arc<str>`; the
/// name carries its own internal representation.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: ToolName,
    pub description: Arc<str>,
    pub parameters: JsonSchema,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {detail}")]
    BadArguments { detail: String },
    #[error("{detail}")]
    Failed { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_rejects_whitespace() {
        assert!("read file".parse::<ToolName>().is_err());
        assert_eq!(
            "read_file".parse::<ToolName>().unwrap().as_str(),
            "read_file"
        );
    }

    #[test]
    fn schema_must_be_an_object() {
        assert!(JsonSchema::try_from(serde_json::json!({"type": "object"})).is_ok());
        assert!(JsonSchema::try_from(serde_json::json!([1, 2])).is_err());
    }
}

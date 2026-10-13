//! Tool vocabulary: names, specs, and errors. Pure data — the async
//! [`agnes_ports::tool::Tool`] port and registry live in `agnes-ports`.

use crate::config::ConfigError;
use std::fmt;
use std::str::FromStr;

/// Name of a tool. Non-empty, no whitespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ToolName(String);

impl ToolName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
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
        Ok(Self(trimmed.to_owned()))
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

/// The contract a model sees: name, description, parameter schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: ToolName,
    pub description: String,
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

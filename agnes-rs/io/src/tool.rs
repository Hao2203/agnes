//! Async tool seam and the registry the driver consults.

use agnes_core::message::{ToolArguments, ToolCall, ToolOutput};
use agnes_core::tool::{ToolError, ToolName, ToolSpec};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// A tool the agent can invoke. Implementations do their own async I/O
/// (filesystem, processes, network…).
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn run(&self, arguments: &ToolArguments) -> Result<ToolOutput, ToolError>;
}

#[derive(Debug, thiserror::Error)]
#[error("duplicate tool name `{0}`")]
pub struct DuplicateTool(pub ToolName);

/// Immutable, name-ordered set of tools.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<ToolName, Arc<dyn Tool>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.tools.keys().map(|n| n.as_str()).collect();
        f.debug_struct("ToolRegistry")
            .field("tools", &names)
            .finish()
    }
}

impl ToolRegistry {
    pub fn build(tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Result<Self, DuplicateTool> {
        let mut map = BTreeMap::new();
        for tool in tools {
            let name = tool.spec().name.clone();
            if map.insert(name.clone(), tool).is_some() {
                return Err(DuplicateTool(name));
            }
        }
        Ok(Self { tools: map })
    }

    pub fn get(&self, name: &ToolName) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Tool specs in deterministic (name) order, so prompts are stable.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec().clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Runs a call, converting every failure into an error [`ToolOutput`] so
    /// the protocol always yields exactly one result per call.
    pub async fn execute(&self, call: &ToolCall) -> ToolOutput {
        match self.tools.get(&call.name) {
            None => ToolOutput::error(format!("unknown tool `{}`", call.name)),
            Some(tool) => tool
                .run(&call.arguments)
                .await
                .unwrap_or_else(|e| ToolOutput::error(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::message::ToolCallId;
    use std::str::FromStr;

    struct Echo {
        spec: ToolSpec,
    }

    impl Echo {
        fn new(name: &str) -> Self {
            Self {
                spec: ToolSpec {
                    name: ToolName::from_str(name).unwrap(),
                    description: "echoes its arguments".into(),
                    parameters: serde_json::json!({"type": "object"}).try_into().unwrap(),
                },
            }
        }
    }

    #[async_trait]
    impl Tool for Echo {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn run(&self, arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::success(arguments.as_str().to_owned()))
        }
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c1"),
            name: ToolName::from_str(name).unwrap(),
            arguments: ToolArguments::new(arguments),
        }
    }

    #[tokio::test]
    async fn registry_rejects_duplicates_and_sorts_specs() {
        ToolRegistry::build(vec![
            Arc::new(Echo::new("read_file")) as Arc<dyn Tool>,
            Arc::new(Echo::new("read_file")) as Arc<dyn Tool>,
        ])
        .unwrap_err();

        let registry = ToolRegistry::build(vec![
            Arc::new(Echo::new("write_file")) as Arc<dyn Tool>,
            Arc::new(Echo::new("read_file")) as Arc<dyn Tool>,
        ])
        .unwrap();
        let specs = registry.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["read_file", "write_file"]);
    }

    #[tokio::test]
    async fn execute_converts_everything_to_output() {
        let registry =
            ToolRegistry::build(vec![Arc::new(Echo::new("echo")) as Arc<dyn Tool>]).unwrap();

        let ok = registry.execute(&call("echo", r#"{"x":1}"#)).await;
        assert!(!ok.is_error);
        assert_eq!(&*ok.content, r#"{"x":1}"#);

        let missing = registry.execute(&call("nope", "{}")).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("unknown tool"));
    }
}

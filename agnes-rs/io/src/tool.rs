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

/// Immutable, name-ordered set of tools. Cloneable so the host can build a
/// candidate set, validate tool changes, and swap it in atomically.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<ToolName, Arc<dyn Tool>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.tools.keys()).finish()
    }
}

impl ToolRegistry {
    /// # Errors
    ///
    /// Returns [`DuplicateTool`] when two tools share a name.
    pub fn build(tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Result<Self, DuplicateTool> {
        let mut registry = Self::default();
        for tool in tools {
            registry.register(tool)?;
        }
        Ok(registry)
    }

    /// Adds a tool; a live registry lets hosts grow the tool set between runs.
    /// A duplicate name leaves the existing tool untouched.
    ///
    /// # Errors
    ///
    /// Returns [`DuplicateTool`] when the name is already registered.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), DuplicateTool> {
        let name = tool.spec().name().clone();
        if self.tools.contains_key(&name) {
            return Err(DuplicateTool(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Removes a tool, returning it when it was there.
    pub fn deregister(&mut self, name: &ToolName) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    /// Whether a tool with this name is registered.
    pub fn contains(&self, name: &ToolName) -> bool {
        self.tools.contains_key(name)
    }

    /// Registered tool names, in deterministic (name) order.
    pub fn names(&self) -> impl Iterator<Item = &ToolName> + '_ {
        self.tools.keys()
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
                spec: ToolSpec::new(
                    ToolName::from_str(name).unwrap(),
                    "echoes its arguments",
                    serde_json::json!({"type": "object"}).try_into().unwrap(),
                ),
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
        let names: Vec<&str> = specs.iter().map(|s| s.name().as_str()).collect();
        assert_eq!(names, ["read_file", "write_file"]);
    }

    #[test]
    fn a_duplicate_registration_keeps_the_existing_tool() {
        let mut registry = ToolRegistry::default();
        let name = ToolName::from_str("echo").unwrap();
        let first = Arc::new(Echo::new("echo")) as Arc<dyn Tool>;
        registry.register(Arc::clone(&first)).unwrap();

        let second = Arc::new(Echo::new("echo")) as Arc<dyn Tool>;
        assert!(registry.register(second).is_err());
        assert!(Arc::ptr_eq(registry.get(&name).unwrap(), &first));
    }

    #[test]
    fn a_live_registry_registers_and_deregisters() {
        let mut registry = ToolRegistry::default();
        let name = ToolName::from_str("echo").unwrap();
        assert!(!registry.contains(&name));

        registry
            .register(Arc::new(Echo::new("echo")) as Arc<dyn Tool>)
            .unwrap();
        assert!(registry.contains(&name));
        assert!(
            registry
                .register(Arc::new(Echo::new("echo")) as Arc<dyn Tool>)
                .is_err()
        );

        assert!(registry.deregister(&name).is_some());
        assert!(!registry.contains(&name));
        assert!(registry.deregister(&name).is_none());
    }

    #[tokio::test]
    async fn execute_converts_everything_to_output() {
        let registry =
            ToolRegistry::build(vec![Arc::new(Echo::new("echo")) as Arc<dyn Tool>]).unwrap();

        let ok = registry.execute(&call("echo", r#"{"x":1}"#)).await;
        assert!(!ok.is_error());
        assert_eq!(ok.content(), r#"{"x":1}"#);

        let missing = registry.execute(&call("nope", "{}")).await;
        assert!(missing.is_error());
        assert!(missing.content().contains("unknown tool"));
    }
}

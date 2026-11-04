use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use super::RegistryError;

/// A tool execution failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    /// The tool could not complete its work.
    #[error("tool failed: {0}")]
    Failed(String),
}

/// A minimal asynchronous tool used by the demo layer.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool name used for registration and lookup.
    fn name(&self) -> &str;

    /// Runs the tool with raw string arguments.
    async fn run(&self, arguments: &str) -> Result<String, ToolError>;
}

/// A set of tools keyed by name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Registers a tool using its own name.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Duplicate`] when the name is already taken.
    pub fn insert(&mut self, tool: Arc<dyn Tool>) -> Result<(), RegistryError> {
        let name = tool.name().to_owned();
        if self.tools.contains_key(&name) {
            return Err(RegistryError::Duplicate { kind: "tool", name });
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Looks up a tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Registered tool names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }
}

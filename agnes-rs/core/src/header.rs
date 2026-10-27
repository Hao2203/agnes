//! Session header: what the model was offered for a turn — the system prompt
//! and the tool environment.
//!
//! The header is plain domain data. The host records its own event when a turn
//! starts or the tool environment changes; the core only uses the header to
//! project a model prompt.

use crate::config::SystemPrompt;
use crate::tool::{ToolName, ToolSpec};
use std::collections::BTreeMap;

/// The model-side configuration in force for a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    system: SystemPrompt,
    environment: Environment,
}

impl Header {
    pub fn new(system: SystemPrompt, environment: Environment) -> Self {
        Self {
            system,
            environment,
        }
    }

    pub fn system(&self) -> &SystemPrompt {
        &self.system
    }

    pub fn environment(&self) -> &Environment {
        &self.environment
    }

    pub(crate) fn environment_mut(&mut self) -> &mut Environment {
        &mut self.environment
    }
}

/// What the run can act with: the tools on offer, keyed by unique name.
///
/// A name appears at most once, so a request can never offer the same tool
/// twice; insertion order does not matter because iteration is name-ordered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Environment {
    tools: BTreeMap<ToolName, ToolSpec>,
}

impl Environment {
    /// Builds the environment from specs; a duplicate name keeps the first
    /// spec, matching the live tool registry's rule.
    pub fn new(tools: impl IntoIterator<Item = ToolSpec>) -> Self {
        let mut environment = Self::default();
        for spec in tools {
            environment.add(spec);
        }
        environment
    }

    /// Adds a spec unless the name is already present. Returns whether the
    /// environment changed.
    pub fn add(&mut self, spec: ToolSpec) -> bool {
        let name = spec.name().clone();
        if self.tools.contains_key(&name) {
            return false;
        }
        self.tools.insert(name, spec);
        true
    }

    /// Removes a tool by name. Returns whether it was there.
    pub fn remove(&mut self, name: &ToolName) -> bool {
        self.tools.remove(name).is_some()
    }

    pub fn contains(&self, name: &ToolName) -> bool {
        self.tools.contains_key(name)
    }

    /// Tool specs in deterministic (name) order.
    pub fn tools(&self) -> impl Iterator<Item = &ToolSpec> + '_ {
        self.tools.values()
    }

    /// Snapshot of the specs, in deterministic order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::JsonSchema;
    use std::str::FromStr;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(
            ToolName::from_str(name).unwrap(),
            "",
            JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        )
    }

    #[test]
    fn duplicate_names_keep_the_first_spec() {
        let mut environment = Environment::new(vec![spec("read_file"), spec("read_file")]);
        assert_eq!(environment.len(), 1);
        assert!(!environment.add(spec("read_file")));
    }

    #[test]
    fn tools_are_iterated_in_name_order() {
        let environment = Environment::new(vec![spec("write_file"), spec("read_file")]);
        let names: Vec<&str> = environment
            .tools()
            .map(|spec| spec.name().as_str())
            .collect();
        assert_eq!(names, ["read_file", "write_file"]);
    }

    #[test]
    fn removal_reports_whether_the_tool_was_there() {
        let mut environment = Environment::new(vec![spec("read_file")]);
        let name = ToolName::from_str("read_file").unwrap();
        assert!(environment.remove(&name));
        assert!(!environment.remove(&name));
        assert!(environment.is_empty());
    }
}

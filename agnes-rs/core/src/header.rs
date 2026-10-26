//! Session header: what the model was offered for a turn — the system prompt
//! and the tool environment.
//!
//! The header is not a field anywhere: it enters the log as
//! [`crate::event::Event::Header`], once at the start of every turn. That
//! makes a request a pure function of the log, and a changed tool set is
//! simply recorded by the next turn instead of being validated out of band.

use crate::config::SystemPrompt;
use crate::tool::ToolSpec;

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
}

/// What the run can act with: the tools on offer. Grows with the needs of
/// hosts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Environment {
    tools: Vec<ToolSpec>,
}

impl Environment {
    pub fn new(tools: Vec<ToolSpec>) -> Self {
        Self { tools }
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }
}

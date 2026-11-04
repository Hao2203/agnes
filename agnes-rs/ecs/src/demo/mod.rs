//! A reference agent layer built on top of the ECS.
//!
//! It is intentionally minimal and independent of the rest of the workspace:
//! models, tools, and permissions are ordinary resources, components, messages,
//! and systems. The core ECS does not know about any of them.

mod http;
mod model;
mod systems;
mod tool;

pub use http::{
    HttpClient, HttpError, HttpMethod, HttpPlugin, HttpRecord, HttpRecordState, HttpRequest,
    HttpRequestSpec, HttpResponse, HttpResponseSpec,
};
pub use model::{ModelError, ModelProvider, ModelRegistry};
pub use tool::{Tool, ToolError, ToolRegistry};

use std::sync::Arc;

use crate::app::{App, Update};
use crate::entity::Entity;
use crate::plugin::Plugin;
use crate::runner::CancelToken;

/// A registry configuration failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// A provider or tool with the same name is already registered.
    #[error("{kind} `{name}` is already registered")]
    Duplicate {
        /// The registry kind, such as `tool`.
        kind: &'static str,
        /// The conflicting name.
        name: String,
    },
}

/// One run of the demo agent.
pub struct AgentRun {
    version: u64,
    cancelled: bool,
    cancellation: CancelToken,
}

impl AgentRun {
    /// Creates a run with a version and its own cancellation token.
    pub fn new(version: u64, cancellation: CancelToken) -> Self {
        Self {
            version,
            cancelled: false,
            cancellation,
        }
    }

    /// The current request version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether the run was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// A clone of the run's cancellation token.
    pub fn cancellation(&self) -> CancelToken {
        self.cancellation.clone()
    }

    pub(crate) fn cancel(&mut self) {
        self.version = self
            .version
            .checked_add(1)
            .expect("run version space exhausted");
        self.cancelled = true;
        self.cancellation.cancel();
    }
}

impl Drop for AgentRun {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// The conversational state of one run.
pub struct Turn {
    run: Entity,
    version: u64,
    prompt: String,
    last_text: String,
    finished: bool,
}

impl Turn {
    /// Creates an unfinished turn.
    pub fn new(run: Entity, version: u64, prompt: String) -> Self {
        Self {
            run,
            version,
            prompt,
            last_text: String::new(),
            finished: false,
        }
    }

    /// The owning run entity.
    pub fn run(&self) -> Entity {
        self.run
    }

    /// The request version this turn belongs to.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The original prompt.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// The latest model or tool text.
    pub fn last_text(&self) -> &str {
        &self.last_text
    }

    /// Whether the turn reached a terminal result.
    pub fn finished(&self) -> bool {
        self.finished
    }

    pub(crate) fn set_text(&mut self, text: String) {
        self.last_text = text;
    }

    pub(crate) fn finish(&mut self) {
        self.finished = true;
    }
}

/// A user prompt entering the runtime.
#[derive(Clone, Debug)]
pub struct UserInput {
    /// The prompt text.
    pub text: String,
}

/// A model completion produced by an async task.
#[derive(Clone, Debug)]
pub struct ModelCompleted {
    /// The owning run.
    pub run: Entity,
    /// The request version the completion belongs to.
    pub version: u64,
    /// The model text.
    pub text: String,
}

/// A tool request extracted from a model completion.
#[derive(Clone, Debug)]
pub struct ToolRequested {
    /// The owning run.
    pub run: Entity,
    /// The request version.
    pub version: u64,
    /// The tool name.
    pub name: String,
    /// Raw tool arguments.
    pub arguments: String,
}

/// A previously requested approval that was granted.
#[derive(Clone, Debug)]
pub struct ToolApproved {
    /// The owning run.
    pub run: Entity,
    /// The request version.
    pub version: u64,
    /// The tool name.
    pub name: String,
    /// Raw tool arguments.
    pub arguments: String,
}

/// A tool result, including denials and failures.
#[derive(Clone, Debug)]
pub struct ToolCompleted {
    /// The owning run.
    pub run: Entity,
    /// The request version.
    pub version: u64,
    /// The tool name.
    pub name: String,
    /// The output or failure text.
    pub output: String,
    /// Whether the tool was refused or failed.
    pub denied: bool,
}

/// An approval request emitted by the permission system.
#[derive(Clone, Debug)]
pub struct ApprovalRequested {
    /// The owning run.
    pub run: Entity,
    /// The request version.
    pub version: u64,
    /// The tool name.
    pub name: String,
    /// Raw tool arguments.
    pub arguments: String,
}

/// A host answer to an approval request.
#[derive(Clone, Debug)]
pub struct ApprovalResolved {
    /// The owning run.
    pub run: Entity,
    /// The request version.
    pub version: u64,
    /// The tool name.
    pub name: String,
    /// Raw tool arguments.
    pub arguments: String,
    /// Whether the call was approved.
    pub approved: bool,
}

/// A request to cancel one run.
#[derive(Clone, Debug)]
pub struct CancelRun {
    /// The run to cancel.
    pub run: Entity,
}

/// A rule matching tool names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Exact name match.
    Name(String),
    /// Prefix match.
    Prefix(String),
}

impl Rule {
    /// Builds an exact-name rule.
    pub fn name(name: impl Into<String>) -> Self {
        Self::Name(name.into())
    }

    /// Builds a prefix rule.
    pub fn prefix(prefix: impl Into<String>) -> Self {
        Self::Prefix(prefix.into())
    }

    /// Whether the rule matches a tool name.
    pub fn matches(&self, tool: &str) -> bool {
        match self {
            Self::Name(name) => name == tool,
            Self::Prefix(prefix) => tool.starts_with(prefix),
        }
    }
}

/// Deny/ask rules evaluated before tool execution.
#[derive(Clone, Debug, Default)]
pub struct PermissionPolicy {
    /// Rules that deny a tool outright.
    pub deny: Vec<Rule>,
    /// Rules that require host approval.
    pub ask: Vec<Rule>,
}

impl PermissionPolicy {
    /// Whether a tool is denied.
    pub fn denies(&self, tool: &str) -> bool {
        self.deny.iter().any(|rule| rule.matches(tool))
    }

    /// Whether a tool requires approval.
    pub fn asks(&self, tool: &str) -> bool {
        self.ask.iter().any(|rule| rule.matches(tool))
    }
}

/// Recorded approval requests, useful for hosts and tests.
#[derive(Default)]
pub struct ApprovalLog {
    /// Requests in arrival order.
    pub requests: Vec<ApprovalRequested>,
}

/// Registers the demo resources and systems.
pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn name(&self) -> &'static str {
        "demo-agent"
    }

    fn build(&self, app: &mut App) {
        app.init_resource::<ModelRegistry>()
            .init_resource::<ToolRegistry>()
            .init_resource::<PermissionPolicy>()
            .init_resource::<ApprovalLog>()
            .add_systems(Update, systems::handle_user_input)
            .add_systems(Update, systems::process_model_response)
            .add_systems(Update, systems::execute_tools)
            .add_systems(Update, systems::handle_approval)
            .add_systems(Update, systems::handle_tool_result)
            .add_systems(Update, systems::cancel_runs)
            .add_systems(Update, systems::record_approvals);
    }
}

/// Registers model providers.
pub struct ModelPlugin {
    providers: Vec<(String, Arc<dyn ModelProvider>)>,
}

impl ModelPlugin {
    /// Registers a provider under its own name.
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        let name = provider.name().to_owned();
        Self {
            providers: vec![(name, provider)],
        }
    }

    /// Registers an additional provider under an explicit name.
    pub fn with_provider(
        mut self,
        name: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
    ) -> Self {
        self.providers.push((name.into(), provider));
        self
    }
}

impl Plugin for ModelPlugin {
    fn name(&self) -> &'static str {
        "demo-model"
    }

    fn build(&self, app: &mut App) {
        app.init_resource::<ModelRegistry>();
        let mut registry = app
            .world_mut()
            .resource_mut::<ModelRegistry>()
            .expect("model registry is installed");
        for (name, provider) in &self.providers {
            registry
                .insert(name.clone(), Arc::clone(provider))
                .unwrap_or_else(|error| panic!("failed to register model provider: {error}"));
        }
    }
}

/// Registers tools.
pub struct ToolPlugin {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolPlugin {
    /// Registers one tool using its own name.
    pub fn new(tool: Arc<dyn Tool>) -> Self {
        Self { tools: vec![tool] }
    }

    /// Registers an empty tool set.
    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    /// Registers an additional tool.
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }
}

impl Plugin for ToolPlugin {
    fn name(&self) -> &'static str {
        "demo-tool"
    }

    fn build(&self, app: &mut App) {
        app.init_resource::<ToolRegistry>();
        let mut registry = app
            .world_mut()
            .resource_mut::<ToolRegistry>()
            .expect("tool registry is installed");
        for tool in &self.tools {
            registry
                .insert(Arc::clone(tool))
                .unwrap_or_else(|error| panic!("failed to register tool: {error}"));
        }
    }
}

/// Installs a permission policy and its gate system.
pub struct PermissionPlugin {
    policy: PermissionPolicy,
}

impl PermissionPlugin {
    /// Creates the plugin from a policy.
    pub fn new(policy: PermissionPolicy) -> Self {
        Self { policy }
    }
}

impl Plugin for PermissionPlugin {
    fn name(&self) -> &'static str {
        "demo-permission"
    }

    fn build(&self, app: &mut App) {
        app.insert_resource(self.policy.clone())
            .add_systems(Update, systems::permission_system);
    }
}

#[cfg(test)]
mod tests;

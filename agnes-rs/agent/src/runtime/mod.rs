//! ECS 运行时：用 [`AgentPlugin`] 把 core 的 typestate 装进消息管线。
//!
//! 机器（[`agnes_core::agent`]）是唯一真相；本模块的系统只是解释阶段
//! 提示、做 IO、记事件。外部装配约定：
//!
//! - `agnes_io::llm::LlmProvider`：provider 插件安装（如
//!   `agnes-plugin-openai`），缺失时第一次模型调用即失败。运行时不碰
//!   网络：它写 `LlmCall` / `LlmCancel` 消息，读 `LlmReply` 消息；
//! - [`RequestBuilderResource`]：宿主安装（模型名在这里决定）；
//! - [`ToolsPlugin`] / `agnes_io::tool::ToolRegistry`：工具表；
//! - `crate::approval::ApprovalChannel`：审批口（配 `ask` 规则时必需，
//!   缺失即 fail-closed 拒绝）；
//! - `crate::permission::PermissionRules`：权限规则（缺省全部放行）；
//! - `agnes_record::Durability`：会话耐久进度（`agnes-plugin-session`
//!   安装；缺省视为无会话、无需等待）；
//! - `agnes_record::Log`：会话日志（本插件兜底初始化；装了
//!   `agnes-plugin-session` 时由它安装恢复的历史并负责轮转重置）；
//! - [`ActiveTurn`]：turn 实体上的零大小标记，宿主可 `Query<&ActiveTurn>`
//!   判断是否有 turn 在飞（输入路由用）。
//!
//! 宿主经 `RuntimeSender` 注入 [`TurnRequested`] / [`UserMessage`] /
//! [`ApprovalDecided`] / [`InterruptTurn`]；收场结果经 [`TurnFinished`]
//! 消息交还宿主。

mod messages;
mod state;
mod systems;

#[cfg(test)]
mod tests;

pub use messages::{
    ApprovalDecided, InterruptTurn, ToolFinished, TurnFailure, TurnFinished, TurnRequested,
    UserMessage,
};
pub use state::{ActiveTurn, PendingTurns, PendingUserMessages};

use std::sync::Arc;

use agnes_core::llm::LlmRequest;
use agnes_core::turn::Turn;
use agnes_ecs::{App, Plugin, Update};
use agnes_io::tool::{Tool, ToolRegistry};
use agnes_record::Log;

use state::PendingCalls;

use crate::RequestBuilder;

/// 把 [`Turn`] 转换成本次请求的宿主转换者，作为资源安装。
///
/// 驱动从不自己组装提示词：[`Turn::prompt`] 只是标准投影，模型名是宿主
/// 的决定。
pub struct RequestBuilderResource(Box<dyn RequestBuilder + Send + Sync>);

impl RequestBuilderResource {
    /// Wraps a request builder.
    pub fn new(builder: Box<dyn RequestBuilder + Send + Sync>) -> Self {
        Self(builder)
    }

    pub(crate) fn build(&self, turn: &Turn) -> LlmRequest {
        self.0.build(turn)
    }
}

/// 注册 agent 运行时：资源兜底 + 按序注册消息管线的全部系统。
///
/// 系统注册顺序即执行顺序；管线的一个 tick 对应旧驱动的一个检查点。
pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn name(&self) -> &'static str {
        "agnes-agent"
    }

    fn build(&self, app: &mut App) {
        app.init_resource::<Log>()
            .init_resource::<ToolRegistry>()
            .init_resource::<PendingTurns>()
            .init_resource::<PendingUserMessages>()
            .init_resource::<PendingCalls>()
            .add_systems(Update, systems::begin_turn)
            .add_systems(Update, systems::handle_interrupt)
            .add_systems(Update, systems::fail_on_session_failure)
            .add_systems(Update, systems::inject_pending)
            .add_systems(Update, systems::issue_model_call)
            .add_systems(Update, systems::process_llm_reply)
            .add_systems(Update, systems::gate_tool_calls)
            .add_systems(Update, systems::resolve_approvals)
            .add_systems(Update, systems::execute_tools)
            .add_systems(Update, systems::finish_tool_calls)
            .add_systems(Update, systems::close_turn);
    }
}

/// 注册工具：每把工具按自己的名字入表。
pub struct ToolsPlugin {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolsPlugin {
    /// 注册一把工具。
    pub fn new(tool: Arc<dyn Tool>) -> Self {
        Self { tools: vec![tool] }
    }

    /// 注册空工具表。
    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    /// 追加一把工具。
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// 追加一批工具。
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        self.tools.extend(tools);
        self
    }
}

impl Plugin for ToolsPlugin {
    fn name(&self) -> &'static str {
        "agnes-tools"
    }

    /// # Panics
    ///
    /// Panics when two tools register under the same name.
    fn build(&self, app: &mut App) {
        app.init_resource::<ToolRegistry>();
        let mut registry = app
            .world_mut()
            .resource_mut::<ToolRegistry>()
            .expect("tool registry is installed");
        for tool in &self.tools {
            if let Err(error) = registry.register(Arc::clone(tool)) {
                panic!("failed to register tool: {error}");
            }
        }
    }
}

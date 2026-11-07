//! ECS 运行时：用 [`AgentPlugin`] 把 core 的 typestate 装进消息管线。
//!
//! 机器（[`agnes_core::agent`]）是唯一真相；本模块的系统只是解释阶段
//! 提示、做 IO、记事件。每会话状态以组件形式挂在 agent 根实体
//! （[`AgentRoot`](crate::AgentRoot) 标记）上，一个世界可以同时运行多个
//! agent；根实体与状态由宿主创建并保管句柄
//! （`agnes_plugin_session::session_state`，加 [`PendingTurns`] /
//! [`PendingUserMessages`] / [`ModelSettings`]），本 crate 的插件不
//! spawn 实体、不挂组件。
//!
//! # 核心状态与能力组件
//!
//! 每个 agent 根实体分两层：
//!
//! - **核心状态（必须）**：`AgentRoot` + `agnes_record::Log` +
//!   [`ModelSettings`] + [`PendingTurns`] +
//!   [`PendingUserMessages`]。缺任一核心组件的 [`TurnRequested`] 不会
//!   panic：`begin_turn` 立即以 [`TurnFinished`] 的 `Err(TurnFailure)`
//!   交还宿主（并记一条 `RunEventKind::Failed`）。
//! - **能力组件（可选，装了就生效）**：`Activity`（观测）、
//!   `Durability` / `SessionPump` / `SessionStoreResource`（会话持久化）
//!   等。能力系统按 `Query<..., With<能力组件>>` 或按实体路由，没有该
//!   组件的 agent 自然跳过。
//!
//! 全局资源（`AsyncExecutor`、`ToolRegistry`、`LlmProvider` 等）是世界
//! 级基础设施，不属于单个 agent 的能力装配。
//!
//! 外部装配约定 —— 强依赖在 build 内断言，注册顺序即每 tick 系统顺序：
//!
//! - `agnes_plugin_session::SessionPlugin`：**先于本插件**（断言）。其
//!   `session_ack` 系统必须在同一 tick 内先于耐久门禁运行；
//! - [`ModelPlugin`]：**先于本插件**（断言）。
//!   `apply_settings` 在同一 tick 内先于模型调用生效；设置组件由宿主挂；
//! - [`ToolsPlugin`]：**先于本插件**（断言）。工具表由它填充；
//! - `agnes_io::llm::LlmProvider`：provider 插件安装（如
//!   `agnes-plugin-openai`）。可替换依赖不断言：`require_provider` 每
//!   tick 检查，缺失时调度当 tick 即报错，不会让调用消失在消息总线里。
//!   运行时不碰网络：它写 `LlmCall` / `LlmCancel` 消息，读 `LlmReply`
//!   消息；
//! - [`RequestBuilderResource`]：宿主安装（提示词投影在这里决定）；
//! - `crate::approval::ApprovalChannel`：审批口（配 `ask` 规则时必需，
//!   缺失即 fail-closed 拒绝）；
//! - `crate::permission::PermissionRules`：权限规则（缺省全部放行）；
//! - `agnes_record::Durability`：会话耐久进度（宿主经 `session_state`
//!   挂到根实体上，`agnes-plugin-session` 的系统驱动它）；
//! - `agnes_record::Log`：会话日志（根实体组件，宿主挂载）；
//! - [`ActiveTurn`] / [`TurnOwner`]：turn 实体上的零大小标记与归属回指，
//!   宿主可 `Query<(&ActiveTurn, &TurnOwner)>` 判断某个 agent 是否有
//!   turn 在飞（输入路由用）。
//!
//! 宿主经 `RuntimeSender` 注入 [`TurnRequested`] / [`UserMessage`] /
//! [`ApprovalDecided`] / [`InterruptTurn`]（前三者与打断都带目标 agent）；
//! 收场结果经 [`TurnFinished`] 消息交还宿主。

mod messages;
mod state;
mod systems;

#[cfg(test)]
mod tests;

pub use messages::{
    ApprovalDecided, InterruptTurn, ToolFinished, TurnFailure, TurnFinished, TurnRequested,
    UserMessage,
};
pub(crate) use state::InFlightCall;
pub use state::{ActiveTurn, PendingTurns, PendingUserMessages, TurnOwner};

use std::sync::Arc;

use agnes_core::llm::LlmRequest;
use agnes_core::turn::Turn;
use agnes_ecs::{App, Plugin, Update};
use agnes_io::tool::{Tool, ToolRegistry};

use state::LlmCallSeq;

use crate::RequestBuilder;
use crate::model::{ModelPlugin, ModelSettings};

/// 把 [`Turn`] 转换成本次请求的宿主转换者，作为资源安装。
///
/// 驱动从不自己组装提示词：[`Turn::prompt`] 只是标准投影，模型与思考
/// 参数来自 [`ModelSettings`]（由 `ModelPlugin` 安装）。
pub struct RequestBuilderResource(Box<dyn RequestBuilder + Send + Sync>);

impl RequestBuilderResource {
    /// Wraps a request builder.
    pub fn new(builder: Box<dyn RequestBuilder + Send + Sync>) -> Self {
        Self(builder)
    }

    pub(crate) fn build(&self, turn: &Turn, settings: &ModelSettings) -> LlmRequest {
        self.0.build(turn, settings)
    }
}

/// 注册 agent 运行时：按序注册消息管线的全部系统。
///
/// 系统注册顺序即执行顺序；管线的一个 tick 对应旧驱动的一个检查点。
/// 根实体与每会话状态由宿主装配，本插件不创建。
pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn name(&self) -> &'static str {
        "agnes-agent"
    }

    /// # Panics
    ///
    /// 注册期断言强依赖：[`SessionPlugin`](agnes_plugin_session::SessionPlugin)、
    /// [`ModelPlugin`] 与 [`ToolsPlugin`] 必须先装。根实体与核心状态由宿主
    /// 装配，本插件不做全局检查：缺核心状态的 turn 请求以失败收场报告。
    fn build(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<agnes_plugin_session::SessionPlugin>(),
            "AgentPlugin requires SessionPlugin to be registered first: \
             its session_ack system must run before the durability gate \
             within the same tick"
        );
        assert!(
            app.is_plugin_added::<ModelPlugin>(),
            "AgentPlugin requires ModelPlugin to be registered first: \
             apply_settings must run before the model call within the same tick"
        );
        assert!(
            app.is_plugin_added::<ToolsPlugin>(),
            "AgentPlugin requires ToolsPlugin to be registered first: \
             the tool registry is filled there"
        );
        app.init_resource::<ToolRegistry>()
            .init_resource::<LlmCallSeq>()
            .add_systems(Update, systems::require_provider)
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

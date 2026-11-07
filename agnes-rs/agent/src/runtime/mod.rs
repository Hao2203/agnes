//! ECS 运行时：用 [`AgentPlugin`] 把 core 的 typestate 装进消息管线。
//!
//! 机器（[`agnes_core::agent`]）是唯一真相；本模块的系统只是解释阶段
//! 提示、做 IO、记事件。每个 agent 是一个挂载
//! [`AgentCore`](crate::AgentCore) 的实体，一个世界可以同时运行多个 agent；
//! 核心状态由宿主用 `AgentCore::new(log, settings)` 创建并保管句柄（日志
//! 来自 `agnes_plugin_session::session_state`），本 crate 的插件不 spawn
//! 实体、不挂组件。
//!
//! # 请求即实体
//!
//! 一个 turn 请求从提交到收场是**同一个实体**：宿主（或 `ingest_requests`）
//! 生成挂 [`TurnRequest`] + [`TurnQueued`] 的实体，调度时在同一实体上插入
//! typestate 阶段与时钟，收场时写入 [`TurnResponse`]。响应不经过消息总线，
//! 因此没有读者、或某个系统提前返回 `Err`，都不会让响应消失。
//!
//! - **请求 id**：[`TurnRequestId`] 由 [`TurnRequestSeq`] 发放，世界与宿主
//!   线程共享同一 id 空间；它也是取消与日志关联的稳定句柄。
//! - **队列**：`Query<(&TurnRequest, &TurnQueued)>` 即队列；同一 agent 每
//!   tick 只启动最早的一条，其余排队。
//! - **终态**：所有路径先写 [`TurnTerminal`]（内部类型），由
//!   `finalize_requests` 统一落成 [`TurnResponse`]（恰好一次）。拉取式宿主
//!   查询组件后 despawn；推送式请求由 [`TurnResponder`] 送达并自动回收。
//!
//! # 核心状态、输入队列与能力组件
//!
//! - **核心状态（必须）**：[`AgentCore`](crate::AgentCore) 一个组件，内含
//!   会话日志与模型设置。没有它的请求不会 panic：`schedule_turns` 立即写
//!   [`TurnResponse`] 的 `Err(TurnFailure { kind: MissingCore, .. })`（并记
//!   一条 `RunEventKind::Failed`）。turn 进行中摘除 `AgentCore` 由
//!   `fail_on_missing_core` 让该 turn 失败收场，不影响其他 agent。
//! - **输入队列（必须）**：[`Inbox`](crate::Inbox) 一个组件，只承载待注入的
//!   用户消息；turn 请求不在这里。
//! - **能力组件（可选，装了就生效）**：`Activity`（观测）、
//!   `Durability` / `SessionPump` / `SessionStoreResource`（会话持久化）
//!   等。能力系统按 `Query<..., With<能力组件>>` 或按实体路由，没有该
//!   组件的 agent 自然跳过。
//!
//! 全局资源（`AsyncExecutor`、`ToolRegistry`、`LlmProvider` 等）是世界
//! 级基础设施，不属于单个 agent 的能力装配。缺 `LlmProvider` 时请求以
//! `Model` 失败收场，而不是让整 tick 返回 `Err`。
//!
//! 外部装配约定 —— 强依赖在 build 内断言，注册顺序即每 tick 系统顺序：
//!
//! - `agnes_plugin_session::SessionPlugin`：**先于本插件**（断言）。其
//!   `session_ack` 系统必须在同一 tick 内先于耐久门禁运行；
//! - [`ModelPlugin`]：**先于本插件**（断言）。
//!   `apply_settings` 在同一 tick 内先于模型调用生效；设置是
//!   [`AgentCore`](crate::AgentCore) 的字段；
//! - [`ToolsPlugin`]：**先于本插件**（断言）。工具表由它填充；
//! - `agnes_io::llm::LlmProvider`：provider 插件安装（如
//!   `agnes-plugin-openai`）。可替换依赖不在注册期断言：缺失时请求以
//!   `Model` 失败终态收场，不会让调用消失在消息总线里，也不会丢响应。
//!   运行时不碰网络：它写 `LlmCall` / `LlmCancel` 消息，读 `LlmReply`
//!   消息；
//! - [`RequestBuilderResource`]：宿主安装（提示词投影在这里决定）；
//! - `crate::approval::ApprovalChannel`：审批口（配 `ask` 规则时必需，
//!   缺失即 fail-closed 拒绝）；
//! - `crate::permission::PermissionRules`：权限规则（缺省全部放行）；
//! - `agnes_record::Durability`：会话耐久进度（宿主经 `session_state`
//!   挂到 agent 实体上，`agnes-plugin-session` 的系统驱动它）；
//! - [`ActiveTurn`] / [`TurnOwner`]：turn 实体上的零大小标记与归属回指，
//!   宿主可 `Query<(&ActiveTurn, &TurnOwner)>` 判断某个 agent 是否有
//!   turn 在飞（输入路由用）。
//!
//! 宿主经 `RuntimeSender` 注入 [`TurnRequested`] / [`UserMessage`] /
//! [`ApprovalDecided`] / [`InterruptTurn`] / [`CancelTurn`]（审批与打断都带
//! 目标 agent）。收场结果不经过消息：查询请求实体上的 [`TurnResponse`]，
//! 或提交时挂 [`TurnResponder`] 等待推送。

mod messages;
pub mod request;
mod state;
mod systems;

#[cfg(test)]
mod tests;

pub use messages::{
    ApprovalDecided, CancelTurn, InterruptTurn, ToolFinished, TurnFailure, TurnFailureKind,
    TurnRequested, UserMessage,
};
pub use request::{
    TurnQueued, TurnRequest, TurnRequestId, TurnRequestSeq, TurnResponder, TurnResponse,
};
pub(crate) use state::InFlightCall;
pub use state::{ActiveTurn, TurnOwner};

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
/// agent 实体与每会话状态由宿主装配，本插件不创建。
pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn name(&self) -> &'static str {
        "agnes-agent"
    }

    /// # Panics
    ///
    /// 注册期断言强依赖：[`SessionPlugin`](agnes_plugin_session::SessionPlugin)、
    /// [`ModelPlugin`] 与 [`ToolsPlugin`] 必须先装。agent 实体与核心状态由宿主
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
            .init_resource::<TurnRequestSeq>()
            .add_systems(Update, systems::session_rotate_log)
            .add_systems(Update, systems::ingest_requests)
            .add_systems(Update, systems::schedule_turns)
            .add_systems(Update, systems::fail_on_missing_core)
            .add_systems(Update, systems::fail_on_session_failure)
            .add_systems(Update, systems::handle_turn_cancels)
            .add_systems(Update, systems::inject_pending)
            .add_systems(Update, systems::issue_model_call)
            .add_systems(Update, systems::process_llm_reply)
            .add_systems(Update, systems::gate_tool_calls)
            .add_systems(Update, systems::resolve_approvals)
            .add_systems(Update, systems::execute_tools)
            .add_systems(Update, systems::finish_tool_calls)
            .add_systems(Update, systems::close_turn)
            .add_systems(Update, systems::fail_on_dropped_phase)
            .add_systems(Update, systems::finalize_requests)
            .add_systems(Update, systems::reap_responded_requests);
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

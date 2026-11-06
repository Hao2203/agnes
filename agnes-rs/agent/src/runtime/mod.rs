//! ECS 运行时：用 [`AgentPlugin`] 把 core 的 typestate 装进消息管线。
//!
//! 机器（[`agnes_core::agent`]）是唯一真相；本模块的系统只是解释阶段
//! 提示、做 IO、记事件。每会话状态以组件形式挂在 agent 根实体
//! （[`AgentRoot`] 标记，[`agent_root`] 可以 find-or-create）上，一个世界
//! 可以同时运行多个 agent。外部装配约定：
//!
//! - `agnes_io::llm::LlmProvider`：provider 插件安装（如
//!   `agnes-plugin-openai`）；`require_provider` 每 tick 检查，缺失时
//!   调度当 tick 即报错，不会让调用消失在消息总线里。运行时不碰
//!   网络：它写 `LlmCall` / `LlmCancel` 消息，读 `LlmReply` 消息；
//! - [`RequestBuilderResource`]：宿主安装（提示词投影在这里决定）；
//! - [`ModelSettings`](crate::model::ModelSettings)：当前模型与思考设置，
//!   `ModelPlugin` 把它挂到 agent 根实体上（必须先于本插件注册：缺失时
//!   debug 构建在注册期断言报错，release 构建由 `issue_model_call` 的
//!   查询参数每 tick 失败）；
//! - [`ToolsPlugin`] / `agnes_io::tool::ToolRegistry`：工具表；
//! - `crate::approval::ApprovalChannel`：审批口（配 `ask` 规则时必需，
//!   缺失即 fail-closed 拒绝）；
//! - `crate::permission::PermissionRules`：权限规则（缺省全部放行）；
//! - `agnes_record::Durability`：会话耐久进度（`agnes-plugin-session`
//!   挂到根实体上；缺省视为无会话、无需等待）；
//! - `agnes_record::Log`：会话日志（根实体组件；本插件兜底初始化，装了
//!   `agnes-plugin-session` 时由它安装恢复的历史并负责轮转重置）；
//! - [`ActiveTurn`]：turn 实体上的零大小标记，宿主可 `Query<&ActiveTurn>`
//!   判断是否有 turn 在飞（输入路由用）。
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
pub use state::{ActiveTurn, PendingTurns, PendingUserMessages};

use std::sync::Arc;

use agnes_core::llm::LlmRequest;
use agnes_core::turn::Turn;
use agnes_ecs::{App, Entity, Plugin, Update, World};
use agnes_io::tool::{Tool, ToolRegistry};
use agnes_record::{AgentRoot, Log};

use state::LlmCallSeq;

use crate::RequestBuilder;
use crate::model::ModelSettings;

/// 该世界的 agent 根实体：已存在的 [`AgentRoot`] 实体，没有就 spawn 一个。
///
/// 插件在安装期用它把自己的每会话状态挂上去；宿主与测试用它拿到
/// 消息里 `agent` 字段要填的实体。多 agent 宿主可以再 `world.spawn(AgentRoot)`
/// 出更多根实体并重复挂载流程。
pub fn agent_root(world: &mut World) -> Entity {
    let existing = world.entity_with::<AgentRoot>();
    existing.unwrap_or_else(|| world.spawn(AgentRoot))
}

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

/// 注册 agent 运行时：资源兜底 + 按序注册消息管线的全部系统。
///
/// 系统注册顺序即执行顺序；管线的一个 tick 对应旧驱动的一个检查点。
pub struct AgentPlugin;

impl Plugin for AgentPlugin {
    fn name(&self) -> &'static str {
        "agnes-agent"
    }

    fn build(&self, app: &mut App) {
        let root = agent_root(app.world_mut());
        // 注册顺序契约：ModelPlugin 必须先装，根实体上才有 ModelSettings，
        // 同一 tick 内设置先于模型调用生效。debug 构建在注册期即报错，
        // 而不是让 `issue_model_call` 的查询参数每 tick 失败。
        debug_assert!(
            app.world().get::<ModelSettings>(root).is_some(),
            "ModelPlugin must be registered before AgentPlugin"
        );
        // 兜底初始化：装了 agnes-plugin-session 时由它先行挂载恢复的
        // 历史，这里只在缺失时补默认值。
        let world = app.world_mut();
        if !world.contains::<Log>(root) {
            world.insert(root, Log::default());
        }
        if !world.contains::<PendingTurns>(root) {
            world.insert(root, PendingTurns::default());
        }
        if !world.contains::<PendingUserMessages>(root) {
            world.insert(root, PendingUserMessages::default());
        }
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

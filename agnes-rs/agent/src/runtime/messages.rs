//! 运行时的消息词汇：宿主注入、异步工具回注、系统间转移。
//!
//! 模型调用不在本模块：`LlmCall` / `LlmReply` / `LlmCancel` 定义在
//! `agnes_io::llm`，provider 插件与运行时共用这套词汇。
//!
//! turn 的**响应**不是消息：它落在请求实体的 [`TurnResponse`] 组件上，
//! 见 [`super::request`]。本模块只保留提交、取消、注入与回注消息。

use agnes_core::content::Content;
use agnes_core::message::{ToolCallId, ToolOutput};
use agnes_core::turn::AgentConfig;
use agnes_ecs::Entity;

use super::request::{TurnRequestId, TurnResponder};

/// 跨线程/外部客户端提交一个 turn 请求。
///
/// 世界里的系统也可以直接调用 [`crate::host::submit_turn`] 生成请求实体；
/// 这条消息是给无法直接访问 `World` 的宿主线程/客户端用的。`id` 由提交方
/// 预先从 `TurnRequestSeq` 取得，便于在响应到达前就持有稳定句柄。
#[derive(Debug)]
pub struct TurnRequested {
    /// 请求 id。
    pub id: TurnRequestId,
    /// 目标 agent 实体。
    pub agent: Entity,
    /// 本轮 turn 的配置。
    pub config: AgentConfig,
    /// 可选的推送出口；`None` 时宿主从请求实体查询 [`TurnResponse`]。
    pub responder: Option<TurnResponder>,
}

/// 运行中注入的用户消息：在目标 agent 的下一次待调用阶段并入会话。
#[derive(Debug)]
pub struct UserMessage {
    /// 消息注入哪个 agent 实体的会话。
    pub agent: Entity,
    pub content: Content,
}

/// 异步工具任务回注：结果（含打断时合成的 `not_run`）。
#[derive(Debug)]
pub struct ToolFinished {
    pub call: Entity,
    pub output: ToolOutput,
}

/// 宿主对一次审批问询的答覆（经 `RuntimeSender` 注入）。
#[derive(Debug)]
pub struct ApprovalDecided {
    /// 答覆属于哪个 agent；`call_id` 只在 agent 内唯一。
    pub agent: Entity,
    pub call_id: ToolCallId,
    pub approved: bool,
}

/// 宿主要打断目标 agent 的当前 turn：in-flight 的模型/工具任务一并取消。
#[derive(Clone, Copy, Debug)]
pub struct InterruptTurn {
    /// 打断哪个 agent 实体的活跃 turn。
    pub agent: Entity,
}

/// 按请求 id 取消：排队中的请求直接以 `Cancelled` 收场；运行中的请求按
/// [`InterruptTurn`] 的路径打断（取消令牌 + `LlmCancel` + 审批落定）。
#[derive(Clone, Copy, Debug)]
pub struct CancelTurn {
    /// 要取消的请求 id。
    pub request: TurnRequestId,
}

/// 一次 turn 的失败类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnFailureKind {
    /// 目标实体缺少 [`AgentCore`](crate::AgentCore)，或 turn 进行中丢失。
    MissingCore,
    /// 模型调用失败（含缺少 provider）。
    Model,
    /// 会话写盘失败（fail-closed）。
    Session,
    /// 请求在排队或运行中被取消。
    Cancelled,
    /// 运行时内部不变量被破坏（防御性收口）。
    Internal,
}

/// 一次 turn 的失败：类别 + 人类可读的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnFailure {
    pub cause: String,
    pub kind: TurnFailureKind,
}

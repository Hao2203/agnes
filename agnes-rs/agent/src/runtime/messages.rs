//! 运行时的消息词汇：宿主注入、异步工具回注、系统间转移。
//!
//! 模型调用不在本模块：`LlmCall` / `LlmReply` / `LlmCancel` 定义在
//! `agnes_io::llm`，provider 插件与运行时共用这套词汇。

use agnes_core::content::Content;
use agnes_core::message::{ToolCallId, ToolOutput};
use agnes_core::turn::TurnResult;
use agnes_ecs::Entity;

/// 宿主要开始一个 turn：目标 agent 与配置（header、goal、预算）一次性给出。
#[derive(Debug)]
pub struct TurnRequested {
    /// 这个 turn 属于哪个 agent 实体。
    pub agent: Entity,
    pub config: AgentConfig,
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
    pub call_id: ToolCallId,
    pub approved: bool,
}

/// 宿主要打断目标 agent 的当前 turn：in-flight 的模型/工具任务一并取消。
#[derive(Clone, Copy, Debug)]
pub struct InterruptTurn {
    /// 打断哪个 agent 实体的活跃 turn。
    pub agent: Entity,
}

/// 一个 turn 收场：正常收场带结果，失败带原因。
#[derive(Debug)]
pub struct TurnFinished {
    /// 收场的 turn 属于哪个 agent 实体。
    pub agent: Entity,
    pub result: Result<TurnResult, TurnFailure>,
    /// turn 的开始与结束时刻（供宿主计算耗时）。
    pub started_at: jiff::Timestamp,
    pub finished_at: jiff::Timestamp,
}

/// 一次 turn 的失败类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnFailureKind {
    /// 目标实体缺少 [`AgentCore`](crate::AgentCore)，或 turn 进行中丢失。
    MissingCore,
    /// 模型调用失败。
    Model,
    /// 会话写盘失败（fail-closed）。
    Session,
}

/// 一次 turn 的失败：类别 + 人类可读的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnFailure {
    pub cause: String,
    pub kind: TurnFailureKind,
}

use agnes_core::turn::AgentConfig;

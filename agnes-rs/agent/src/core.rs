//! agent 实体的核心状态与宿主输入队列。
//!
//! 核心状态收成**一个** [`AgentCore`] 组件：实体挂上它就是 agent，同时拥有
//! 会话日志与模型设置。部分装配在类型层面不可表示，运行时不再需要在每个
//! 系统里分别检查 `Log` / `ModelSettings` 是否存在。
//!
//! [`Inbox`] 是宿主输入队列（turn 请求、用户消息），独立于核心状态：核心
//! 状态丢失时，队列仍可被排空并把积压请求以失败交还宿主，而不是无声消失。
//!
//! 能力组件（`Activity`、`Durability` / `SessionPump` /
//! `SessionStoreResource` 等）同样独立挂载：装了就由对应系统按组件过滤
//! 消费，没装的 agent 自然跳过。

use std::sync::Arc;

use agnes_core::content::Content;
use agnes_record::Log;

use crate::model::ModelSettings;
use crate::runtime::TurnRequested;

/// 一个 agent 实体的核心状态。
///
/// 一个组件承载会话日志（[`Log`]）与模型设置（[`ModelSettings`]）。
///
/// 它是 agent 的身份本身：`TurnRequested` 指向没有 `AgentCore` 的实体时以
/// [`TurnFinished`](crate::runtime::TurnFinished) 的失败结果收场，不会 panic。
/// 核心状态在 turn 生命周期内视为不可变；在 turn 进行中摘除 `AgentCore`
/// 会让该 turn 以失败收场，而不是中止整个调度。
pub struct AgentCore {
    log: Log,
    settings: ModelSettings,
}

impl AgentCore {
    /// 创建一个 agent 的核心状态。
    pub fn new(log: Log, settings: ModelSettings) -> Self {
        Self { log, settings }
    }

    /// 会话日志。
    pub fn log(&self) -> &Log {
        &self.log
    }

    /// 模型设置。
    pub fn settings(&self) -> &ModelSettings {
        &self.settings
    }

    pub(crate) fn log_mut(&mut self) -> &mut Log {
        &mut self.log
    }

    pub(crate) fn settings_mut(&mut self) -> &mut ModelSettings {
        &mut self.settings
    }
}

/// 一个 agent 实体的宿主输入队列。
///
/// turn 请求在 agent 忙时排队，用户消息在下一个待调用边界前等待。队列与
/// [`AgentCore`] 分开存放：核心状态被摘除时，系统仍能排空队列并把积压请求
/// 以失败交还宿主，避免调用方永久等待。
#[derive(Default)]
pub struct Inbox {
    turns: Vec<Arc<TurnRequested>>,
    messages: Vec<Content>,
}

impl Inbox {
    /// 取走等待注入的用户消息（宿主收场时归还输入侧）。
    pub fn take_pending_messages(&mut self) -> Vec<Content> {
        std::mem::take(&mut self.messages)
    }

    pub(crate) fn push_turn(&mut self, request: Arc<TurnRequested>) {
        self.turns.push(request);
    }

    pub(crate) fn pop_turn(&mut self) -> Option<Arc<TurnRequested>> {
        (!self.turns.is_empty()).then(|| self.turns.remove(0))
    }

    pub(crate) fn take_turns(&mut self) -> Vec<Arc<TurnRequested>> {
        std::mem::take(&mut self.turns)
    }

    pub(crate) fn push_message(&mut self, content: Content) {
        self.messages.push(content);
    }
}

agnes_ecs::component!(AgentCore, Inbox);

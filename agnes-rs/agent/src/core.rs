//! agent 实体的核心状态与宿主输入队列。
//!
//! 核心状态收成**一个** [`AgentCore`] 组件：实体挂上它就是 agent，同时拥有
//! 会话日志与模型设置。部分装配在类型层面不可表示，运行时不再需要在每个
//! 系统里分别检查 `Log` / `ModelSettings` 是否存在。
//!
//! [`Inbox`] 是运行中用户消息的输入队列。turn 请求不再是队列里的数据：
//! 每个请求是一个带 [`TurnRequest`](crate::runtime::TurnRequest) 与
//! [`TurnQueued`](crate::runtime::TurnQueued) 的实体，核心状态丢失时由调度
//! 系统给它们写 [`TurnResponse`](crate::runtime::TurnResponse)，不会无声消失。
//!
//! 能力组件（`Activity`、`Durability` / `SessionPump` /
//! `SessionStoreResource` 等）同样独立挂载：装了就由对应系统按组件过滤
//! 消费，没装的 agent 自然跳过。

use agnes_core::content::Content;
use agnes_record::Log;

use crate::model::ModelSettings;

/// 一个 agent 实体的核心状态。
///
/// 一个组件承载会话日志（[`Log`]）与模型设置（[`ModelSettings`]）。
///
/// 它是 agent 的身份本身：请求指向没有 `AgentCore` 的实体时，请求实体上会
/// 得到 [`TurnResponse`](crate::runtime::TurnResponse) 的失败结果，不会
/// panic。核心状态在 turn 生命周期内视为不可变；在 turn 进行中摘除
/// `AgentCore` 会让该 turn 以失败收场，而不是中止整个调度。
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

/// 一个 agent 实体的运行中用户消息队列。
///
/// 消息只在目标 agent 的下一个待调用边界前等待。队列与 [`AgentCore`] 分开
/// 存放：核心状态被摘除时，队列仍可被宿主取回。turn 请求不在这里 —— 它们是
/// 独立实体，排队状态由 `TurnQueued` 标记表达。
#[derive(Default)]
pub struct Inbox {
    messages: Vec<Content>,
}

impl Inbox {
    /// 取走等待注入的用户消息（宿主收场时归还输入侧）。
    pub fn take_pending_messages(&mut self) -> Vec<Content> {
        std::mem::take(&mut self.messages)
    }

    pub(crate) fn push_message(&mut self, content: Content) {
        self.messages.push(content);
    }
}

agnes_ecs::component!(AgentCore, Inbox);

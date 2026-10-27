//! 驱动状态机的动态货币：接线、typestate 封装、等待点与落点。

use std::time::Duration;

use agnes_core::header::Header;
use agnes_core::llm::{LlmRequest, LlmResponse};
use agnes_core::message::{Message, ToolCall, ToolCallId, ToolOutput};
use agnes_core::turn::{
    Action, AwaitingMessage, AwaitingTools, Closed, MessageReplied, ToolsAdvanced, Turn,
};
use agnes_io::input::Input as InputPort;
use agnes_io::llm::Llm;
use agnes_io::session::SessionLog;
use agnes_io::tool::ToolRegistry;
use tokio::sync::oneshot::Receiver as AnswerReceiver;

use crate::approval::ApprovalChannel;
use crate::ext::{ApprovalOutcome, Extensions};
use crate::{RequestBuilder, Sink};

/// 一次驱动的固定接线：全部被动 IO 端与策略注入点。
pub(crate) struct Ports<'a> {
    /// 工具注册表可变：插件可以在下一个模型调用前增删工具。
    pub(crate) registry: &'a mut ToolRegistry,
    pub(crate) llm: &'a dyn Llm,
    pub(crate) input: &'a dyn InputPort,
    pub(crate) requests: &'a dyn RequestBuilder,
    pub(crate) extensions: &'a mut Extensions,
    pub(crate) approvals: Option<&'a ApprovalChannel>,
}

/// 一次 run 的出口：上报与落盘只在这次 run 内借用。
pub(crate) struct Outlets<'s, 'q> {
    pub(crate) sink: &'s mut dyn Sink,
    pub(crate) session: Option<&'q mut dyn SessionLog>,
}
/// core turn 的阶段化封装 —— 驱动状态机的动态货币。
/// （core 保持自己的 typestate；驱动需要在运行时以动态状态持有它。）
pub(super) enum StagedTurn {
    AwaitingMessage(Turn<AwaitingMessage>),
    AwaitingTools(Turn<AwaitingTools>),
    Closed(Turn<Closed>),
}

impl StagedTurn {
    pub(super) fn poll_actions(&mut self) -> Option<Action> {
        match self {
            StagedTurn::AwaitingMessage(turn) => turn.poll_actions(),
            StagedTurn::AwaitingTools(turn) => turn.poll_actions(),
            StagedTurn::Closed(turn) => turn.poll_actions(),
        }
    }

    pub(super) fn next_action(&self) -> Option<&Action> {
        match self {
            StagedTurn::AwaitingMessage(turn) => turn.next_action(),
            StagedTurn::AwaitingTools(turn) => turn.next_action(),
            StagedTurn::Closed(turn) => turn.next_action(),
        }
    }

    /// 下一个动作是否会启动新的工作（模型调用 / 工具派发）。
    pub(super) fn is_about_to_work(&self) -> bool {
        matches!(
            self.next_action(),
            Some(Action::CallModel | Action::RunTool(_))
        )
    }

    /// 下一个动作是否就是待发的模型调用。
    pub(super) fn is_about_to_call_model(&self) -> bool {
        matches!(self.next_action(), Some(Action::CallModel))
    }

    pub(super) fn is_open(&self) -> bool {
        !matches!(self, StagedTurn::Closed(_))
    }

    pub(super) fn inject(&mut self, message: Message) {
        match self {
            StagedTurn::AwaitingMessage(turn) => turn.inject(message),
            StagedTurn::AwaitingTools(turn) => turn.inject(message),
            StagedTurn::Closed(_) => {
                debug_assert!(false, "a closed turn cannot accept injected messages");
            }
        }
    }

    /// 在下一次模型调用前替换环境；只有队首是 `CallModel` 时才能做。
    pub(super) fn set_environment(&mut self, header: Header) -> bool {
        match self {
            StagedTurn::AwaitingMessage(turn) => turn.set_environment(header),
            _ => false,
        }
    }

    pub(super) fn tool_started(&mut self) {
        match self {
            StagedTurn::AwaitingTools(turn) => turn.tool_started(),
            _ => debug_assert!(false, "只有等工具的机器能开始执行调用"),
        }
    }

    pub(super) fn model_responded(self, response: LlmResponse) -> Self {
        match self {
            StagedTurn::AwaitingMessage(turn) => match turn.model_responded(response) {
                MessageReplied::Tools { turn } => StagedTurn::AwaitingTools(turn),
                MessageReplied::Continued { turn } => StagedTurn::AwaitingMessage(turn),
                MessageReplied::Closed { turn } => StagedTurn::Closed(turn),
            },
            other => {
                debug_assert!(false, "等工具/已收场的机器不接模型响应");
                other
            }
        }
    }

    pub(super) fn tool_finished(self, call_id: ToolCallId, output: ToolOutput) -> Self {
        match self {
            StagedTurn::AwaitingTools(turn) => match turn.tool_finished(call_id, output) {
                ToolsAdvanced::Ongoing { turn } => StagedTurn::AwaitingTools(turn),
                ToolsAdvanced::NextStep { turn } => StagedTurn::AwaitingMessage(turn),
                ToolsAdvanced::Closed { turn } => StagedTurn::Closed(turn),
            },
            other => {
                debug_assert!(false, "等消息/已收场的机器不接工具结果");
                other
            }
        }
    }

    pub(super) fn interrupted(self) -> Self {
        match self {
            StagedTurn::AwaitingMessage(turn) => StagedTurn::Closed(turn.interrupted()),
            StagedTurn::AwaitingTools(turn) => StagedTurn::Closed(turn.interrupted()),
            closed @ StagedTurn::Closed(_) => closed,
        }
    }
}
/// 驱动在等什么。
pub(super) enum Waiting {
    /// 等模型响应。
    Model { request: LlmRequest },
    /// 等工具执行结果（已放行）。
    Tool { call: ToolCall },
    /// 等审批答覆（可选超时）。
    Approval {
        call: ToolCall,
        answer: AnswerReceiver<bool>,
        timeout: Option<Duration>,
    },
}

/// 泵完动作后的三种合法落点。
pub(super) enum Pumped {
    /// 动作已排空，但既没有等待点也没有收场 —— 审批放行后、执行工具前。
    Idle(StagedTurn),
    /// 机器摆好了一个等待点。
    Waiting(StagedTurn, Waiting),
    /// 机器已经收场。
    Closed(Turn<Closed>),
}

/// 挂起的审批问询如何落定。
pub(super) enum Resolution {
    /// 批准一次 —— 照常执行。
    Allowed,
    /// 拒绝；`outcome` 是 fail-closed 词汇，`detail` 是给模型与界面的理由。
    Denied {
        outcome: ApprovalOutcome,
        detail: &'static str,
    },
    /// 等待被打断 —— 整个 run 收场。
    Interrupted,
}

impl Resolution {
    pub(super) fn outcome(&self) -> ApprovalOutcome {
        match self {
            Resolution::Allowed => ApprovalOutcome::AllowedOnce,
            Resolution::Denied { outcome, .. } => *outcome,
            Resolution::Interrupted => ApprovalOutcome::Cancelled,
        }
    }
}

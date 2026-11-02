//! 下游记录词汇：一次 run 的持久化事件与宿主审计记录。
//!
//! 记录格式归宿主所有 —— core 只持有领域状态（[`Turn`]），不产出事件。
//! 驱动按发生顺序铸造这些事件并写入日志；会话格式、插件观察、界面渲染
//! 都使用这一套词汇。`HostEvent` 是 core 不该知道的宿主语义（审批、中止、
//! 失败），与事件共享同一条顺序。
//!
//! [`Turn`]: agnes_core::turn::Turn

use agnes_core::config::ModelName;
use agnes_core::header::Header;
use agnes_core::llm::StopKind;
use agnes_core::message::{Message, TokenUsage, ToolCall, ToolCallId};
use agnes_core::tool::ToolName;
use agnes_core::turn::{Outcome, StepNumber};

use crate::approval::ApprovalOutcome;
use crate::ext::Origin;

/// 一个 step 的结局。只属于记录词汇：core 不再需要它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// 模型给出最终答复。
    Answered,
    /// 模型要求工具，step 在工具轮结算。
    ToolUse,
    /// 输出触顶。
    LengthExceeded,
    /// 内容过滤器中止响应。
    ContentFiltered,
    /// 用户打断。
    Interrupted,
}

impl From<StopKind> for StepOutcome {
    fn from(stop: StopKind) -> Self {
        match stop {
            StopKind::Completed => Self::Answered,
            StopKind::ToolUse => Self::ToolUse,
            StopKind::LengthExceeded => Self::LengthExceeded,
            StopKind::ContentFiltered => Self::ContentFiltered,
        }
    }
}

/// 运行时日志里的一条，按发生顺序。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 模型侧配置（系统提示与工具环境）生效：每个 turn 开局记一条；
    /// 工具环境运行中变化时再记一条。
    Header(Header),
    /// turn 开始：一条用户请求进入 agent 循环。
    TurnStart,
    /// 一个 step 开始（一次模型往返）。
    StepStart { step: StepNumber },
    /// step 开始生成模型消息。
    MessageStart { step: StepNumber },
    /// 一条消息进入会话：goal、注入的消息、模型消息、工具结果。
    Message(Message),
    /// step 的模型消息完成。
    MessageEnd {
        step: StepNumber,
        /// 产生这条消息的模型（驱动从它构造的请求回带）。
        model: ModelName,
        stop: StopKind,
        usage: TokenUsage,
    },
    /// 一个工具调用开始执行。
    ToolExecutionStart { step: StepNumber, call: ToolCall },
    /// 一个工具调用执行完毕；结果事件紧随其后作为 [`Event::Message`]。
    ToolExecutionEnd {
        step: StepNumber,
        call_id: ToolCallId,
    },
    /// 一个 step 收尾。
    StepEnd {
        step: StepNumber,
        outcome: StepOutcome,
    },
    /// 运行中在模型调用前注入的消息数量（消息本身紧随其后逐条作为
    /// [`Event::Message`] 入账）。
    MessagesInjected { count: usize },
    /// turn 收场 —— 结果就是整个 run 的结果。
    TurnEnd { outcome: Outcome },
}

/// 宿主审计记录，与 [`Event`] 共享同一条顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// An approval question was handed to the answerer.
    ApprovalRequested { call_id: ToolCallId, tool: ToolName },
    /// The question settled (including cancellation and unavailability).
    ApprovalResolved {
        call_id: ToolCallId,
        outcome: ApprovalOutcome,
    },
    /// A plugin aborted the run; the turn closes as interrupted, but the
    /// cause belongs to the host, not to core.
    Aborted { by: Origin, reason: String },
    /// The run stopped with an error (model, transport or session write).
    /// Core does not model I/O failures, so the cause lives here.
    Failed { cause: String },
}

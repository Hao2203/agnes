//! 运行时事件词汇 —— 一条按发生顺序记录的日志，消息也是事件。
//!
//! 会话里发生的一切都是一个 [`Event`]，由 [`Context`](crate::context::Context)
//! 按发生顺序记账：turn 边界、模型往返（step）边界、流式增量、工具执行、
//! 以及消息本身（[`Event::Message`]）。因为消息也是事件，
//! `Context::events()` 任何时候都能复原完整的先后顺序。
//!
//! 提示：turn/step 的生命周期事件都由机器在推进时铸造；唯一的例外
//! 是工具执行的开始/结束 —— 只有宿主知道调用什么时候真开始、真结束
//! （它把 [`crate::turn::Input::ToolStarted`] / 结果喂回机器）。

use crate::content::Content;
use crate::llm::StopReason;
use crate::message::{Message, MessageDelta, TokenUsage, ToolCall, ToolCallId};
use crate::step::{StepNumber, StepOutcome};
use crate::turn::Outcome;

/// 运行时日志里的一条，按发生顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// turn 开始：一条用户请求进入 agent 循环。
    TurnStart,
    /// 一个 step 开始（一次模型往返）。
    StepStart { step: StepNumber },
    /// step 开始生成模型消息。
    MessageStart { step: StepNumber },
    /// 正在生成的消息的一段增量（流式适配器才有）。
    MessageUpdate {
        step: StepNumber,
        delta: MessageDelta,
    },
    /// 一条消息进入会话：goal、注入的用户消息、模型消息、工具结果。
    Message(Message),
    /// step 的模型消息完成。
    MessageEnd {
        step: StepNumber,
        stop: StopReason,
        usage: TokenUsage,
    },
    /// 一个工具调用开始执行。
    ToolExecutionStart { step: StepNumber, call: ToolCall },
    /// 一个工具调用执行完毕；结果事件紧随其后作为 [`Event::Message`]。
    ToolExecutionEnd {
        step: StepNumber,
        call_id: ToolCallId,
        is_error: bool,
    },
    /// 一个 step 收尾。
    StepEnd {
        step: StepNumber,
        outcome: StepOutcome,
    },
    /// 运行中被插话、在 step 边界生效的用户消息（随后逐条作为
    /// [`Event::Message`] 入账）。
    UserMessagesInjected { messages: Vec<Content> },
    /// turn 收场 —— 结果就是整个 run 的结果。
    TurnEnd { outcome: Outcome },
}

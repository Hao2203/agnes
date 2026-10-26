//! 运行时事件词汇 —— 一条按发生顺序记录的日志，消息与配置也是事件。
//!
//! 会话里发生的一切都是一个 [`Event`]，由机器作为动作交给下游，按发生
//! 顺序记入日志：模型侧配置（[`Event::Header`]）、turn 边界、模型往返
//! （step）边界、工具执行、以及消息本身（[`Event::Message`]）。因为一切
//! 都是事件，日志任何时候都能复原完整的先后顺序，一次请求也能从事件序列
//! 单独推导出来。
//!
//! 提示：turn/step 的生命周期事件都由机器在推进时铸造；唯一的例外
//! 是工具执行的开始/结束 —— 只有宿主知道调用什么时候真开始、真结束
//! （它用 [`crate::turn::Turn::tool_started`] /
//! [`crate::turn::Turn::tool_finished`] 喂回机器）。

use crate::header::Header;
use crate::llm::StopKind;
use crate::message::{Message, TokenUsage, ToolCall, ToolCallId};
use crate::turn::{Outcome, StepNumber, StepOutcome};

/// 运行时日志里的一条，按发生顺序。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 模型侧配置（系统提示与工具环境）生效：每个 turn 开局记一条，
    /// 日志里最后一条就是这一轮请求的依据。
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
        /// 产生这条消息的模型（适配器从请求回带）。
        model: crate::config::ModelName,
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
    /// 运行中被注入、在 step 边界生效的消息数量（消息本身紧随其后逐条
    /// 作为 [`Event::Message`] 入账，不在这里重复携带）。
    MessagesInjected { count: usize },
    /// turn 收场 —— 结果就是整个 run 的结果。
    TurnEnd { outcome: Outcome },
}

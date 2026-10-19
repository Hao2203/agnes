//! 运行时事件词汇 —— 一条按发生顺序记录的日志，消息也是事件。
//!
//! 会话里发生的一切都是一个 [`Event`]，由 [`Context`](crate::context::Context)
//! 按发生顺序记账：turn 边界、模型往返（step）边界、工具执行、
//! 以及消息本身（[`Event::Message`]）。因为消息也是事件，
//! `Context::events()` 任何时候都能复原完整的先后顺序。
//!
//! 提示：turn/step 的生命周期事件都由机器在推进时铸造；唯一的例外
//! 是工具执行的开始/结束 —— 只有宿主知道调用什么时候真开始、真结束
//! （它用 [`crate::turn::Turn::tool_started`] /
//! [`crate::turn::Turn::tool_finished`] 喂回机器）。

use crate::content::Content;
use crate::llm::StopReason;
use crate::message::{Message, TokenUsage, ToolCall, ToolCallId};
use crate::turn::{Outcome, StepNumber, StepOutcome};

/// 运行时日志里的一条，按发生顺序。
///
/// 序列化是耐久日志的格式契约：变体名是 `snake_case`，未知变体在反序列
/// 化时被拒绝（fail closed）—— 读不懂的日志不猜，直接拒载。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// turn 开始：一条用户请求进入 agent 循环。
    TurnStart,
    /// 一个 step 开始（一次模型往返）。
    StepStart { step: StepNumber },
    /// step 开始生成模型消息。
    MessageStart { step: StepNumber },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemPrompt;
    use crate::message::{AssistantMessage, ToolOutput};
    use crate::tool::ToolName;
    use agnes_util::NonEmpty;
    use std::str::FromStr;

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(id),
            name: ToolName::from_str("note").unwrap(),
            arguments: crate::message::ToolArguments::new(r#"{"text":"hi"}"#),
        }
    }

    /// 每个变体各来一条，覆盖嵌套的消息与结果形状。
    fn every_variant() -> Vec<Event> {
        let step = StepNumber::FIRST;
        let first_call = call("c1");
        vec![
            Event::TurnStart,
            Event::StepStart { step },
            Event::MessageStart { step },
            Event::Message(Message::System {
                content: SystemPrompt::new("sys"),
            }),
            Event::Message(Message::User {
                content: content("go"),
            }),
            Event::Message(Message::Assistant(AssistantMessage::Text(content("hi")))),
            Event::Message(Message::Assistant(AssistantMessage::ToolCalls {
                content: None,
                calls: NonEmpty::new(first_call.clone()),
            })),
            Event::Message(Message::ToolResult {
                call_id: first_call.id.clone(),
                output: ToolOutput::success("noted"),
            }),
            Event::MessageEnd {
                step,
                stop: StopReason::ToolUse {
                    content: None,
                    calls: NonEmpty::new(first_call),
                },
                usage: TokenUsage { input: 3, output: 4 },
            },
            Event::ToolExecutionStart {
                step,
                call: call("c2"),
            },
            Event::ToolExecutionEnd {
                step,
                call_id: ToolCallId::new("c2"),
                is_error: true,
            },
            Event::StepEnd {
                step,
                outcome: StepOutcome::ToolUse,
            },
            Event::UserMessagesInjected {
                messages: vec![content("also")],
            },
            Event::TurnEnd {
                outcome: Outcome::Answered {
                    reply: content("done"),
                },
            },
        ]
    }

    #[test]
    fn every_event_variant_round_trips() {
        for event in every_variant() {
            let json = serde_json::to_string(&event).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(back, event, "round trip failed for {json}");
        }
    }

    #[test]
    fn invalid_payloads_are_refused() {
        // 空内容
        assert!(serde_json::from_str::<Content>(r#""""#).is_err());
        // 空工具调用集合 —— NonEmpty 的构造器不可绕过
        assert!(
            serde_json::from_str::<StopReason>(r#"{"tool_use":{"content":null,"calls":[]}}"#)
                .is_err()
        );
        // step 编号不能为 0
        assert!(serde_json::from_str::<Event>(r#"{"step_start":{"step":0}}"#).is_err());
        // 工具名不能带空白
        assert!(serde_json::from_str::<ToolName>(r#""a b""#).is_err());
        // 参数模式必须是对象
        assert!(serde_json::from_str::<crate::tool::JsonSchema>("[1,2]").is_err());
        // 未知事件变体：读不懂就拒载，不静默丢弃
        assert!(serde_json::from_str::<Event>(r#"{"future_event":{}}"#).is_err());
    }
}

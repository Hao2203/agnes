//! 会话文件的线格式：v4 JSONL 的具体形状，以及与领域词汇的互转。
//!
//! 格式是本 crate 的私事 —— core 的类型不实现序列化，形状、校验与拒载
//! 规则都定义在这里（与 `agnes-llm-openai` 的 wire 同一模式）。变体名是
//! `snake_case`，未知变体在反序列化时被拒绝（fail closed）—— 读不懂的
//! 日志不猜，直接拒载；形状非法（空内容、空工具调用集合、step 0、带空白
//! 的工具名、非对象的参数模式）在换回领域类型时同样拒载。

use agnes_core::config::{ModelName, SystemPrompt};
use agnes_core::content::Content;
use agnes_core::event::Event;
use agnes_core::header::{Environment, Header};
use agnes_core::llm::StopReason;
use agnes_core::message::{
    AssistantMessage, Message, TokenUsage, ToolArguments, ToolCall, ToolCallId, ToolOutput,
};
use agnes_core::tool::{JsonSchema, ToolName, ToolSpec};
use agnes_core::turn::{Outcome, StepNumber, StepOutcome};
use agnes_io::session::{ApprovalOutcome, HostEvent};
use agnes_util::NonEmpty;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::str::FromStr;

/// 一行日志：版本标记、core 记录或宿主审计记录。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Line {
    Meta(Meta),
    Core(CoreRecord),
    Host(HostRecord),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Meta {
    pub format: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CoreRecord {
    pub seq: u64,
    pub time: Timestamp,
    pub event: WireEvent,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct HostRecord {
    pub seq: u64,
    pub time: Timestamp,
    pub event: WireHostEvent,
}

/// [`HostEvent`] 的线形状。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireHostEvent {
    ApprovalRequested {
        call_id: String,
        tool: String,
    },
    ApprovalResolved {
        call_id: String,
        outcome: WireApprovalOutcome,
    },
    Failed {
        cause: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

/// [`Event`] 的线形状。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireEvent {
    Header(WireHeader),
    TurnStart,
    StepStart {
        step: NonZeroU32,
    },
    MessageStart {
        step: NonZeroU32,
    },
    Message(WireMessage),
    MessageEnd {
        step: NonZeroU32,
        model: String,
        stop: WireStopReason,
        usage: WireTokenUsage,
    },
    ToolExecutionStart {
        step: NonZeroU32,
        call: WireToolCall,
    },
    ToolExecutionEnd {
        step: NonZeroU32,
        call_id: String,
        is_error: bool,
    },
    StepEnd {
        step: NonZeroU32,
        outcome: WireStepOutcome,
    },
    MessagesInjected {
        messages: Vec<WireMessage>,
    },
    TurnEnd {
        outcome: WireOutcome,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WireHeader {
    pub system: String,
    pub environment: WireEnvironment,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WireEnvironment {
    pub tools: Vec<WireToolSpec>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WireToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant(WireAssistantMessage),
    ToolResult {
        call_id: String,
        output: WireToolOutput,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireAssistantMessage {
    Text(String),
    ToolCalls {
        content: Option<String>,
        calls: Vec<WireToolCall>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WireToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WireToolOutput {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WireTokenUsage {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireStopReason {
    Completed {
        content: String,
    },
    ToolUse {
        content: Option<String>,
        calls: Vec<WireToolCall>,
    },
    LengthExceeded {
        content: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireStepOutcome {
    Answered { reply: String },
    ToolUse,
    LengthExceeded,
    Interrupted,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireOutcome {
    Answered { reply: String },
    LengthExceeded,
    Interrupted,
    BudgetExhausted,
}

// —— 领域 → 线：无损 ——

impl From<&Event> for WireEvent {
    fn from(event: &Event) -> Self {
        match event {
            Event::Header(header) => Self::Header(header.into()),
            Event::TurnStart => Self::TurnStart,
            Event::StepStart { step } => Self::StepStart {
                step: step_of(*step),
            },
            Event::MessageStart { step } => Self::MessageStart {
                step: step_of(*step),
            },
            Event::Message(message) => Self::Message(message.into()),
            Event::MessageEnd {
                step,
                model,
                stop,
                usage,
            } => Self::MessageEnd {
                step: step_of(*step),
                model: model.as_str().to_owned(),
                stop: stop.into(),
                usage: usage.into(),
            },
            Event::ToolExecutionStart { step, call } => Self::ToolExecutionStart {
                step: step_of(*step),
                call: call.into(),
            },
            Event::ToolExecutionEnd {
                step,
                call_id,
                is_error,
            } => Self::ToolExecutionEnd {
                step: step_of(*step),
                call_id: call_id.as_str().to_owned(),
                is_error: *is_error,
            },
            Event::StepEnd { step, outcome } => Self::StepEnd {
                step: step_of(*step),
                outcome: outcome.into(),
            },
            Event::MessagesInjected { messages } => Self::MessagesInjected {
                messages: messages.iter().map(Into::into).collect(),
            },
            Event::TurnEnd { outcome } => Self::TurnEnd {
                outcome: outcome.into(),
            },
        }
    }
}

fn step_of(step: StepNumber) -> NonZeroU32 {
    NonZeroU32::new(step.get()).expect("a step number is never zero")
}

fn step_from(raw: NonZeroU32) -> StepNumber {
    StepNumber::new(raw)
}

impl From<&Header> for WireHeader {
    fn from(header: &Header) -> Self {
        Self {
            system: header.system.as_str().to_owned(),
            environment: (&header.environment).into(),
        }
    }
}

impl From<&Environment> for WireEnvironment {
    fn from(environment: &Environment) -> Self {
        Self {
            tools: environment.tools().iter().map(Into::into).collect(),
        }
    }
}

impl From<&ToolSpec> for WireToolSpec {
    fn from(spec: &ToolSpec) -> Self {
        Self {
            name: spec.name.as_str().to_owned(),
            description: spec.description.to_string(),
            parameters: spec.parameters.get().clone(),
        }
    }
}

impl From<&Message> for WireMessage {
    fn from(message: &Message) -> Self {
        match message {
            Message::System { content } => Self::System {
                content: content.as_str().to_owned(),
            },
            Message::User { content } => Self::User {
                content: content.as_str().to_owned(),
            },
            Message::Assistant(assistant) => Self::Assistant(assistant.into()),
            Message::ToolResult { call_id, output } => Self::ToolResult {
                call_id: call_id.as_str().to_owned(),
                output: WireToolOutput {
                    content: output.content.to_string(),
                    is_error: output.is_error,
                },
            },
        }
    }
}

impl From<&AssistantMessage> for WireAssistantMessage {
    fn from(message: &AssistantMessage) -> Self {
        match message {
            AssistantMessage::Text(content) => Self::Text(content.as_str().to_owned()),
            AssistantMessage::ToolCalls { content, calls } => Self::ToolCalls {
                content: content.as_ref().map(|content| content.as_str().to_owned()),
                calls: calls.iter().map(Into::into).collect(),
            },
        }
    }
}

impl From<&ToolCall> for WireToolCall {
    fn from(call: &ToolCall) -> Self {
        Self {
            id: call.id.as_str().to_owned(),
            name: call.name.as_str().to_owned(),
            arguments: call.arguments.as_str().to_owned(),
        }
    }
}

impl From<&StopReason> for WireStopReason {
    fn from(stop: &StopReason) -> Self {
        match stop {
            StopReason::Completed { content } => Self::Completed {
                content: content.as_str().to_owned(),
            },
            StopReason::ToolUse { content, calls } => Self::ToolUse {
                content: content.as_ref().map(|content| content.as_str().to_owned()),
                calls: calls.iter().map(Into::into).collect(),
            },
            StopReason::LengthExceeded { content } => Self::LengthExceeded {
                content: content.as_ref().map(|content| content.as_str().to_owned()),
            },
        }
    }
}

impl From<&TokenUsage> for WireTokenUsage {
    fn from(usage: &TokenUsage) -> Self {
        Self {
            input: usage.input,
            output: usage.output,
        }
    }
}

impl From<&StepOutcome> for WireStepOutcome {
    fn from(outcome: &StepOutcome) -> Self {
        match outcome {
            StepOutcome::Answered { reply } => Self::Answered {
                reply: reply.as_str().to_owned(),
            },
            StepOutcome::ToolUse => Self::ToolUse,
            StepOutcome::LengthExceeded => Self::LengthExceeded,
            StepOutcome::Interrupted => Self::Interrupted,
        }
    }
}

impl From<&Outcome> for WireOutcome {
    fn from(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Answered { reply } => Self::Answered {
                reply: reply.as_str().to_owned(),
            },
            Outcome::LengthExceeded => Self::LengthExceeded,
            Outcome::Interrupted => Self::Interrupted,
            Outcome::BudgetExhausted => Self::BudgetExhausted,
        }
    }
}

impl From<&HostEvent> for WireHostEvent {
    fn from(event: &HostEvent) -> Self {
        match event {
            HostEvent::ApprovalRequested { call_id, tool } => Self::ApprovalRequested {
                call_id: call_id.as_str().to_owned(),
                tool: tool.as_str().to_owned(),
            },
            HostEvent::ApprovalResolved { call_id, outcome } => Self::ApprovalResolved {
                call_id: call_id.as_str().to_owned(),
                outcome: (*outcome).into(),
            },
            HostEvent::Failed { cause } => Self::Failed {
                cause: cause.clone(),
            },
        }
    }
}

impl From<ApprovalOutcome> for WireApprovalOutcome {
    fn from(outcome: ApprovalOutcome) -> Self {
        match outcome {
            ApprovalOutcome::AllowedOnce => Self::AllowedOnce,
            ApprovalOutcome::Rejected => Self::Rejected,
            ApprovalOutcome::Cancelled => Self::Cancelled,
            ApprovalOutcome::Unavailable => Self::Unavailable,
        }
    }
}

// —— 线 → 领域：fail closed ——

impl TryFrom<WireEvent> for Event {
    type Error = String;

    fn try_from(event: WireEvent) -> Result<Self, Self::Error> {
        Ok(match event {
            WireEvent::Header(header) => Self::Header(header.try_into()?),
            WireEvent::TurnStart => Self::TurnStart,
            WireEvent::StepStart { step } => Self::StepStart {
                step: step_from(step),
            },
            WireEvent::MessageStart { step } => Self::MessageStart {
                step: step_from(step),
            },
            WireEvent::Message(message) => Self::Message(message.try_into()?),
            WireEvent::MessageEnd {
                step,
                model,
                stop,
                usage,
            } => Self::MessageEnd {
                step: step_from(step),
                model: ModelName::from_str(&model).map_err(|err| err.to_string())?,
                stop: stop.try_into()?,
                usage: usage.into(),
            },
            WireEvent::ToolExecutionStart { step, call } => Self::ToolExecutionStart {
                step: step_from(step),
                call: call.try_into()?,
            },
            WireEvent::ToolExecutionEnd {
                step,
                call_id,
                is_error,
            } => Self::ToolExecutionEnd {
                step: step_from(step),
                call_id: ToolCallId::new(call_id),
                is_error,
            },
            WireEvent::StepEnd { step, outcome } => Self::StepEnd {
                step: step_from(step),
                outcome: outcome.try_into()?,
            },
            WireEvent::MessagesInjected { messages } => Self::MessagesInjected {
                messages: messages
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<Vec<_>, _>>()?,
            },
            WireEvent::TurnEnd { outcome } => Self::TurnEnd {
                outcome: outcome.try_into()?,
            },
        })
    }
}

impl TryFrom<WireHeader> for Header {
    type Error = String;

    fn try_from(header: WireHeader) -> Result<Self, Self::Error> {
        Ok(Self {
            system: SystemPrompt::new(header.system),
            environment: Environment::new(
                header
                    .environment
                    .tools
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        })
    }
}

impl TryFrom<WireToolSpec> for ToolSpec {
    type Error = String;

    fn try_from(spec: WireToolSpec) -> Result<Self, Self::Error> {
        Ok(Self {
            name: tool_name(spec.name)?,
            description: spec.description.into(),
            parameters: JsonSchema::try_from(spec.parameters).map_err(|err| err.to_string())?,
        })
    }
}

impl TryFrom<WireMessage> for Message {
    type Error = String;

    fn try_from(message: WireMessage) -> Result<Self, Self::Error> {
        Ok(match message {
            WireMessage::System { content } => Self::System {
                content: SystemPrompt::new(content),
            },
            WireMessage::User { content: raw } => Self::User {
                content: content_from(raw)?,
            },
            WireMessage::Assistant(assistant) => Self::Assistant(assistant.try_into()?),
            WireMessage::ToolResult { call_id, output } => Self::ToolResult {
                call_id: ToolCallId::new(call_id),
                output: ToolOutput {
                    content: output.content.into(),
                    is_error: output.is_error,
                },
            },
        })
    }
}

impl TryFrom<WireAssistantMessage> for AssistantMessage {
    type Error = String;

    fn try_from(message: WireAssistantMessage) -> Result<Self, Self::Error> {
        Ok(match message {
            WireAssistantMessage::Text(text) => Self::Text(content_from(text)?),
            WireAssistantMessage::ToolCalls {
                content: raw,
                calls,
            } => Self::ToolCalls {
                content: raw.map(content_from).transpose()?,
                calls: tool_calls(calls)?,
            },
        })
    }
}

impl TryFrom<WireToolCall> for ToolCall {
    type Error = String;

    fn try_from(call: WireToolCall) -> Result<Self, Self::Error> {
        Ok(Self {
            id: ToolCallId::new(call.id),
            name: tool_name(call.name)?,
            arguments: ToolArguments::new(call.arguments),
        })
    }
}

impl TryFrom<WireStopReason> for StopReason {
    type Error = String;

    fn try_from(stop: WireStopReason) -> Result<Self, Self::Error> {
        Ok(match stop {
            WireStopReason::Completed { content: raw } => Self::Completed {
                content: content_from(raw)?,
            },
            WireStopReason::ToolUse {
                content: raw,
                calls,
            } => Self::ToolUse {
                content: raw.map(content_from).transpose()?,
                calls: tool_calls(calls)?,
            },
            WireStopReason::LengthExceeded { content: raw } => Self::LengthExceeded {
                content: raw.map(content_from).transpose()?,
            },
        })
    }
}

impl From<WireTokenUsage> for TokenUsage {
    fn from(usage: WireTokenUsage) -> Self {
        Self {
            input: usage.input,
            output: usage.output,
        }
    }
}

impl TryFrom<WireStepOutcome> for StepOutcome {
    type Error = String;

    fn try_from(outcome: WireStepOutcome) -> Result<Self, Self::Error> {
        Ok(match outcome {
            WireStepOutcome::Answered { reply } => Self::Answered {
                reply: content_from(reply)?,
            },
            WireStepOutcome::ToolUse => Self::ToolUse,
            WireStepOutcome::LengthExceeded => Self::LengthExceeded,
            WireStepOutcome::Interrupted => Self::Interrupted,
        })
    }
}

impl TryFrom<WireOutcome> for Outcome {
    type Error = String;

    fn try_from(outcome: WireOutcome) -> Result<Self, Self::Error> {
        Ok(match outcome {
            WireOutcome::Answered { reply } => Self::Answered {
                reply: content_from(reply)?,
            },
            WireOutcome::LengthExceeded => Self::LengthExceeded,
            WireOutcome::Interrupted => Self::Interrupted,
            WireOutcome::BudgetExhausted => Self::BudgetExhausted,
        })
    }
}

impl TryFrom<WireHostEvent> for HostEvent {
    type Error = String;

    fn try_from(event: WireHostEvent) -> Result<Self, Self::Error> {
        Ok(match event {
            WireHostEvent::ApprovalRequested { call_id, tool } => Self::ApprovalRequested {
                call_id: ToolCallId::new(call_id),
                tool: tool_name(tool)?,
            },
            WireHostEvent::ApprovalResolved { call_id, outcome } => Self::ApprovalResolved {
                call_id: ToolCallId::new(call_id),
                outcome: outcome.into(),
            },
            WireHostEvent::Failed { cause } => Self::Failed { cause },
        })
    }
}

impl From<WireApprovalOutcome> for ApprovalOutcome {
    fn from(outcome: WireApprovalOutcome) -> Self {
        match outcome {
            WireApprovalOutcome::AllowedOnce => Self::AllowedOnce,
            WireApprovalOutcome::Rejected => Self::Rejected,
            WireApprovalOutcome::Cancelled => Self::Cancelled,
            WireApprovalOutcome::Unavailable => Self::Unavailable,
        }
    }
}

fn content_from(raw: String) -> Result<Content, String> {
    Content::try_from(raw).map_err(|err| err.to_string())
}

fn tool_name(raw: String) -> Result<ToolName, String> {
    ToolName::from_str(&raw).map_err(|err| err.to_string())
}

fn tool_calls(calls: Vec<WireToolCall>) -> Result<NonEmpty<ToolCall>, String> {
    let calls = calls
        .into_iter()
        .map(TryInto::try_into)
        .collect::<Result<Vec<_>, _>>()?;
    NonEmpty::from_vec(calls).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::content::Content;
    use agnes_core::message::AssistantMessage;
    use agnes_core::tool::ToolName;
    use agnes_util::NonEmpty;

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(id),
            name: ToolName::from_str("note").unwrap(),
            arguments: ToolArguments::new(r#"{"text":"hi"}"#),
        }
    }

    /// 每个变体各来一条，覆盖嵌套的消息与结果形状。
    fn every_variant() -> Vec<Event> {
        let step = StepNumber::FIRST;
        let first_call = call("c1");
        vec![
            Event::Header(Header::new(
                SystemPrompt::new("sys"),
                Environment::new(vec![ToolSpec {
                    name: ToolName::from_str("read_file").unwrap(),
                    description: "reads".into(),
                    parameters: JsonSchema::try_from(serde_json::json!({"type": "object"}))
                        .unwrap(),
                }]),
            )),
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
                model: ModelName::from_str("test-model").unwrap(),
                stop: StopReason::ToolUse {
                    content: None,
                    calls: NonEmpty::new(first_call),
                },
                usage: TokenUsage {
                    input: 3,
                    output: 4,
                },
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
            Event::MessagesInjected {
                messages: vec![
                    Message::User {
                        content: content("also"),
                    },
                    Message::System {
                        content: SystemPrompt::new("injected note"),
                    },
                ],
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
            let json = serde_json::to_string(&WireEvent::from(&event)).unwrap();
            let wire: WireEvent = serde_json::from_str(&json).unwrap();
            let back = Event::try_from(wire).unwrap();
            assert_eq!(back, event, "round trip failed for {json}");
        }
    }
}

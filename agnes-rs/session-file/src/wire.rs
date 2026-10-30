//! 会话文件的线格式：v6 JSONL 的具体形状，以及与领域词汇的互转。
//!
//! 格式是本 crate 的私事 —— core 的类型不实现序列化，形状、校验与拒载
//! 规则都定义在这里（与 `agnes-llm-openai` 的 wire 同一模式）。变体名是
//! `snake_case`，未知变体在反序列化时被拒绝（fail closed）—— 读不懂的
//! 日志不猜，直接拒载；形状非法（空内容、空工具调用集合、step 0、带空白
//! 的工具名、非对象的参数模式）在换回领域类型时同样拒载。

use agnes_agent::approval::ApprovalOutcome;
use agnes_agent::record::{Event, HostEvent, StepOutcome};
use agnes_core::config::{ConfigError, ModelName, SystemPrompt};
use agnes_core::content::Content;
use agnes_core::header::{Environment, Header};
use agnes_core::llm::StopKind;
use agnes_core::message::{
    AssistantMessage, Message, TokenUsage, ToolArguments, ToolCall, ToolCallId, ToolOutput,
};
use agnes_core::tool::{JsonSchema, SchemaError, ToolName, ToolSpec};
use agnes_core::turn::Outcome;
use agnes_util::NonEmpty;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::str::FromStr;

/// Why a wire payload does not describe a legal domain value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WireError {
    #[error("content must not be empty")]
    EmptyContent,
    #[error("a tool-use message must carry at least one call")]
    EmptyToolCalls,
    #[error("tool call id must not be empty")]
    EmptyCallId,
    #[error("tool name is invalid")]
    ToolName(#[source] ConfigError),
    #[error("model name is invalid")]
    ModelName(#[source] ConfigError),
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error("tool `{name}` is declared twice in one environment")]
    DuplicateTool { name: String },
}

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
    Aborted {
        by: String,
        reason: String,
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
        stop: WireStopKind,
        usage: WireTokenUsage,
    },
    ToolExecutionStart {
        step: NonZeroU32,
        call: WireToolCall,
    },
    ToolExecutionEnd {
        step: NonZeroU32,
        call_id: String,
    },
    StepEnd {
        step: NonZeroU32,
        outcome: WireStepOutcome,
    },
    MessagesInjected {
        count: usize,
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
pub(crate) enum WireStopKind {
    Completed,
    ToolUse,
    LengthExceeded,
    ContentFiltered,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireStepOutcome {
    Answered,
    ToolUse,
    LengthExceeded,
    ContentFiltered,
    Interrupted,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireOutcome {
    Answered,
    LengthExceeded,
    ContentFiltered,
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
                step: (*step).into(),
            },
            Event::MessageStart { step } => Self::MessageStart {
                step: (*step).into(),
            },
            Event::Message(message) => Self::Message(message.into()),
            Event::MessageEnd {
                step,
                model,
                stop,
                usage,
            } => Self::MessageEnd {
                step: (*step).into(),
                model: model.as_str().to_owned(),
                stop: (*stop).into(),
                usage: usage.into(),
            },
            Event::ToolExecutionStart { step, call } => Self::ToolExecutionStart {
                step: (*step).into(),
                call: call.into(),
            },
            Event::ToolExecutionEnd { step, call_id } => Self::ToolExecutionEnd {
                step: (*step).into(),
                call_id: call_id.as_str().to_owned(),
            },
            Event::StepEnd { step, outcome } => Self::StepEnd {
                step: (*step).into(),
                outcome: outcome.into(),
            },
            Event::MessagesInjected { count } => Self::MessagesInjected { count: *count },
            Event::TurnEnd { outcome } => Self::TurnEnd {
                outcome: outcome.into(),
            },
        }
    }
}

impl From<&Header> for WireHeader {
    fn from(header: &Header) -> Self {
        Self {
            system: header.system().as_str().to_owned(),
            environment: header.environment().into(),
        }
    }
}

impl From<&Environment> for WireEnvironment {
    fn from(environment: &Environment) -> Self {
        Self {
            tools: environment.tools().map(Into::into).collect(),
        }
    }
}

impl From<&ToolSpec> for WireToolSpec {
    fn from(spec: &ToolSpec) -> Self {
        Self {
            name: spec.name().as_str().to_owned(),
            description: spec.description().to_owned(),
            parameters: spec.parameters().as_value().clone(),
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
                    content: output.content().to_owned(),
                    is_error: output.is_error(),
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

impl From<StopKind> for WireStopKind {
    fn from(stop: StopKind) -> Self {
        match stop {
            StopKind::Completed => Self::Completed,
            StopKind::ToolUse => Self::ToolUse,
            StopKind::LengthExceeded => Self::LengthExceeded,
            StopKind::ContentFiltered => Self::ContentFiltered,
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
            StepOutcome::Answered => Self::Answered,
            StepOutcome::ToolUse => Self::ToolUse,
            StepOutcome::LengthExceeded => Self::LengthExceeded,
            StepOutcome::ContentFiltered => Self::ContentFiltered,
            StepOutcome::Interrupted => Self::Interrupted,
        }
    }
}

impl From<&Outcome> for WireOutcome {
    fn from(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Answered => Self::Answered,
            Outcome::LengthExceeded => Self::LengthExceeded,
            Outcome::ContentFiltered => Self::ContentFiltered,
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
            HostEvent::Aborted { by, reason } => Self::Aborted {
                by: by.as_str().to_owned(),
                reason: reason.clone(),
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
    type Error = WireError;

    fn try_from(event: WireEvent) -> Result<Self, Self::Error> {
        Ok(match event {
            WireEvent::Header(header) => Self::Header(header.try_into()?),
            WireEvent::TurnStart => Self::TurnStart,
            WireEvent::StepStart { step } => Self::StepStart { step: step.into() },
            WireEvent::MessageStart { step } => Self::MessageStart { step: step.into() },
            WireEvent::Message(message) => Self::Message(message.try_into()?),
            WireEvent::MessageEnd {
                step,
                model,
                stop,
                usage,
            } => Self::MessageEnd {
                step: step.into(),
                model: ModelName::from_str(&model).map_err(WireError::ModelName)?,
                stop: stop.into(),
                usage: usage.into(),
            },
            WireEvent::ToolExecutionStart { step, call } => Self::ToolExecutionStart {
                step: step.into(),
                call: call.try_into()?,
            },
            WireEvent::ToolExecutionEnd { step, call_id } => Self::ToolExecutionEnd {
                step: step.into(),
                call_id: ToolCallId::new(call_id),
            },
            WireEvent::StepEnd { step, outcome } => Self::StepEnd {
                step: step.into(),
                outcome: outcome.try_into()?,
            },
            WireEvent::MessagesInjected { count } => Self::MessagesInjected { count },
            WireEvent::TurnEnd { outcome } => Self::TurnEnd {
                outcome: outcome.try_into()?,
            },
        })
    }
}

impl TryFrom<WireHeader> for Header {
    type Error = WireError;

    fn try_from(header: WireHeader) -> Result<Self, Self::Error> {
        let mut environment = Environment::default();
        for tool in header.environment.tools {
            let spec = ToolSpec::try_from(tool)?;
            let name = spec.name().as_str().to_owned();
            if !environment.add(spec) {
                return Err(WireError::DuplicateTool { name });
            }
        }
        Ok(Header::new(SystemPrompt::new(header.system), environment))
    }
}

impl TryFrom<WireToolSpec> for ToolSpec {
    type Error = WireError;

    fn try_from(spec: WireToolSpec) -> Result<Self, Self::Error> {
        Ok(ToolSpec::new(
            tool_name(spec.name)?,
            spec.description,
            JsonSchema::try_from(spec.parameters)?,
        ))
    }
}

impl TryFrom<WireMessage> for Message {
    type Error = WireError;

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
                output: if output.is_error {
                    ToolOutput::error(output.content)
                } else {
                    ToolOutput::success(output.content)
                },
            },
        })
    }
}

impl TryFrom<WireAssistantMessage> for AssistantMessage {
    type Error = WireError;

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
    type Error = WireError;

    fn try_from(call: WireToolCall) -> Result<Self, Self::Error> {
        if call.id.is_empty() {
            return Err(WireError::EmptyCallId);
        }
        Ok(Self {
            id: ToolCallId::new(call.id),
            name: tool_name(call.name)?,
            arguments: ToolArguments::new(call.arguments),
        })
    }
}

impl From<WireStopKind> for StopKind {
    fn from(stop: WireStopKind) -> Self {
        match stop {
            WireStopKind::Completed => Self::Completed,
            WireStopKind::ToolUse => Self::ToolUse,
            WireStopKind::LengthExceeded => Self::LengthExceeded,
            WireStopKind::ContentFiltered => Self::ContentFiltered,
        }
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
    type Error = WireError;

    fn try_from(outcome: WireStepOutcome) -> Result<Self, Self::Error> {
        Ok(match outcome {
            WireStepOutcome::Answered => Self::Answered,
            WireStepOutcome::ToolUse => Self::ToolUse,
            WireStepOutcome::LengthExceeded => Self::LengthExceeded,
            WireStepOutcome::ContentFiltered => Self::ContentFiltered,
            WireStepOutcome::Interrupted => Self::Interrupted,
        })
    }
}

impl TryFrom<WireOutcome> for Outcome {
    type Error = WireError;

    fn try_from(outcome: WireOutcome) -> Result<Self, Self::Error> {
        Ok(match outcome {
            WireOutcome::Answered => Self::Answered,
            WireOutcome::LengthExceeded => Self::LengthExceeded,
            WireOutcome::ContentFiltered => Self::ContentFiltered,
            WireOutcome::Interrupted => Self::Interrupted,
            WireOutcome::BudgetExhausted => Self::BudgetExhausted,
        })
    }
}

impl TryFrom<WireHostEvent> for HostEvent {
    type Error = WireError;

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
            WireHostEvent::Aborted { by, reason } => Self::Aborted {
                by: by.into(),
                reason,
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

fn content_from(raw: String) -> Result<Content, WireError> {
    Content::try_from(raw).map_err(|_| WireError::EmptyContent)
}

fn tool_name(raw: String) -> Result<ToolName, WireError> {
    ToolName::from_str(&raw).map_err(WireError::ToolName)
}

fn tool_calls(calls: Vec<WireToolCall>) -> Result<NonEmpty<ToolCall>, WireError> {
    let mut converted = Vec::with_capacity(calls.len());
    for call in calls {
        converted.push(call.try_into()?);
    }
    NonEmpty::from_vec(converted).map_err(|_| WireError::EmptyToolCalls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::content::Content;
    use agnes_core::message::AssistantMessage;
    use agnes_core::tool::ToolName;
    use agnes_core::turn::StepNumber;
    use agnes_util::NonEmpty;

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    #[test]
    fn duplicate_tool_names_in_a_header_are_refused() {
        let tool = || WireToolSpec {
            name: "read_file".into(),
            description: "reads".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let header = WireHeader {
            system: "sys".into(),
            environment: WireEnvironment {
                tools: vec![tool(), tool()],
            },
        };
        assert!(matches!(
            Header::try_from(header),
            Err(WireError::DuplicateTool { name }) if name == "read_file"
        ));
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
                Environment::new(vec![ToolSpec::new(
                    ToolName::from_str("read_file").unwrap(),
                    "reads",
                    JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
                )]),
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
                stop: StopKind::ToolUse,
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
            },
            Event::StepEnd {
                step,
                outcome: StepOutcome::ToolUse,
            },
            Event::MessagesInjected { count: 2 },
            Event::TurnEnd {
                outcome: Outcome::Answered,
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

    #[test]
    fn every_host_event_variant_round_trips() {
        let events = vec![
            HostEvent::ApprovalRequested {
                call_id: ToolCallId::new("c1"),
                tool: ToolName::from_str("run_shell").unwrap(),
            },
            HostEvent::ApprovalResolved {
                call_id: ToolCallId::new("c1"),
                outcome: ApprovalOutcome::Rejected,
            },
            HostEvent::Aborted {
                by: "policy".into(),
                reason: "stop here".into(),
            },
            HostEvent::Failed {
                cause: "down".into(),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&WireHostEvent::from(&event)).unwrap();
            let wire: WireHostEvent = serde_json::from_str(&json).unwrap();
            let back = HostEvent::try_from(wire).unwrap();
            assert_eq!(back, event, "round trip failed for {json}");
        }
    }
}

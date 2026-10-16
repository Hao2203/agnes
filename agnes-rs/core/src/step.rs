//! 一次模型往返（step）：请求出去 →（流式增量）→ 模型消息 →（要工具？）
//! → 工具逐个执行 → 收尾。
//!
//! [`Step`] 是 turn 机器内部的类型状态子机（外层是 [`crate::turn::Turn`]，
//! 事件驱动地按阶段分发输入）：每个转移消耗当前阶段、把发生的事写进
//! [`Context`] 的有序日志、交还同一批事件供报告。非法顺序不编译 ——
//! 等消息的 step 没有工具方法，等工具的 step 没有增量/响应方法。
//!
//! 工具执行是宿主的事：机器按模型给出的顺序逐个派发调用；宿主每执行完
//! 一个就把 `tool_execution_start` / 结果喂回 turn 机器，由它落到这里。
//! 工具失败也是结果：错误作为 [`ToolOutput`] 进入日志，由模型看见。

use crate::content::Content;
use crate::context::Context;
use crate::event::Event;
use crate::llm::{LlmResponse, StopReason};
use crate::message::{AssistantMessage, Message, MessageDelta, ToolCall, ToolCallId, ToolOutput};
use agnes_util::NonEmpty;
use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

/// step 编号（turn 内 1 起始）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct StepNumber(NonZeroU32);

impl StepNumber {
    pub const FIRST: Self = Self(NonZeroU32::MIN);

    pub fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for StepNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 一个 step 怎么收尾。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// 模型给出最终答复：turn 随之收场。
    Answered { reply: Content },
    /// 模型要工具：turn 开下一个 step 接着走。
    ToolUse,
    /// 模型输出触顶。
    LengthExceeded,
    /// 用户打断。
    Interrupted,
}

/// 一次模型往返，以类型状态推进。
#[derive(Debug)]
pub struct Step<P> {
    phase: P,
}

/// 阶段：等模型消息（增量 / 响应）。
#[derive(Debug)]
pub struct Generating {
    number: StepNumber,
}

/// 阶段：等工具结果 —— 调用按模型给出的顺序派发给宿主，结果按 id 落位。
#[derive(Debug)]
pub struct Executing {
    number: StepNumber,
    calls: NonEmpty<ToolCall>,
    /// 已交给宿主的调用数。
    dispatched: usize,
    outputs: BTreeMap<ToolCallId, ToolOutput>,
}

/// `message_ended` 的走向。
#[derive(Debug)]
#[must_use]
pub enum StepReply {
    /// 模型要工具：step 进入执行阶段，turn 把这些调用逐个交出去。
    Tools {
        step: Step<Executing>,
        events: Vec<Event>,
    },
    /// 模型给出最终答复：step 收尾，turn 随之收场。
    Answered { reply: Content, events: Vec<Event> },
    /// 模型输出触顶。
    LengthExceeded { events: Vec<Event> },
}

/// 记录事件并把同一批交还；每个转移经此落账。
fn record(context: &mut Context, events: Vec<Event>) -> Vec<Event> {
    for event in &events {
        context.record(event.clone());
    }
    events
}

impl Step<Generating> {
    /// 开启 step：`step_start`、`message_start`。
    pub fn begin(number: StepNumber, context: &mut Context) -> (Self, Vec<Event>) {
        let events = record(
            context,
            vec![
                Event::StepStart { step: number },
                Event::MessageStart { step: number },
            ],
        );
        (
            Self {
                phase: Generating { number },
            },
            events,
        )
    }

    pub fn number(&self) -> StepNumber {
        self.phase.number
    }

    /// 一段流式增量：`message_update`。
    pub fn update(&self, context: &mut Context, delta: MessageDelta) -> Event {
        let event = Event::MessageUpdate {
            step: self.phase.number,
            delta,
        };
        context.record(event.clone());
        event
    }

    /// 模型消息完成：`message_end`，随后要么转入工具执行，
    /// 要么（答复 / 触顶）收尾这个 step。
    pub fn message_ended(self, context: &mut Context, response: LlmResponse) -> StepReply {
        let number = self.phase.number;
        let LlmResponse { stop_reason, usage } = response;
        let stop = stop_reason.clone();
        match stop_reason {
            StopReason::ToolUse { content, calls } => {
                let events = record(
                    context,
                    vec![
                        Event::Message(Message::Assistant(AssistantMessage::ToolCalls {
                            content,
                            calls: calls.clone(),
                        })),
                        Event::MessageEnd {
                            step: number,
                            stop,
                            usage,
                        },
                    ],
                );
                StepReply::Tools {
                    step: Step {
                        phase: Executing {
                            number,
                            calls,
                            dispatched: 0,
                            outputs: BTreeMap::new(),
                        },
                    },
                    events,
                }
            }
            StopReason::Completed { content } => {
                let reply = content.clone();
                let events = record(
                    context,
                    vec![
                        Event::Message(Message::Assistant(AssistantMessage::Text(content))),
                        Event::MessageEnd {
                            step: number,
                            stop,
                            usage,
                        },
                        Event::StepEnd {
                            step: number,
                            outcome: StepOutcome::Answered {
                                reply: reply.clone(),
                            },
                        },
                    ],
                );
                StepReply::Answered { reply, events }
            }
            StopReason::LengthExceeded { content } => {
                let mut events = Vec::with_capacity(3);
                if let Some(content) = content {
                    events.push(Event::Message(Message::Assistant(AssistantMessage::Text(
                        content,
                    ))));
                }
                events.push(Event::MessageEnd {
                    step: number,
                    stop,
                    usage,
                });
                events.push(Event::StepEnd {
                    step: number,
                    outcome: StepOutcome::LengthExceeded,
                });
                StepReply::LengthExceeded {
                    events: record(context, events),
                }
            }
        }
    }

    /// 用户打断（响应未达）：`step_end{Interrupted}`。被丢弃的消息从未发生。
    pub fn interrupted(self, context: &mut Context) -> Vec<Event> {
        record(
            context,
            vec![Event::StepEnd {
                step: self.phase.number,
                outcome: StepOutcome::Interrupted,
            }],
        )
    }
}

impl Step<Executing> {
    pub fn number(&self) -> StepNumber {
        self.phase.number
    }

    /// 把下一个调用交给宿主执行；全部派发完则返回 None。
    pub fn take_next(&mut self) -> Option<ToolCall> {
        let call = self
            .phase
            .calls
            .as_ref()
            .get(self.phase.dispatched)?
            .clone();
        self.phase.dispatched += 1;
        Some(call)
    }

    /// 宿主开始执行：`tool_execution_start`。宿主只应喂已派发的调用；
    /// 未知 id 被忽略。
    pub fn tool_started(&self, context: &mut Context, call_id: &ToolCallId) -> Option<Event> {
        let call = self
            .phase
            .calls
            .iter()
            .find(|call| call.id == *call_id)?
            .clone();
        let event = Event::ToolExecutionStart {
            step: self.phase.number,
            call,
        };
        context.record(event.clone());
        Some(event)
    }

    /// 宿主完成执行：`tool_execution_end`，结果消息紧随其后入账。
    /// 未知 id 被忽略。
    pub fn tool_finished(
        &mut self,
        context: &mut Context,
        call_id: ToolCallId,
        output: ToolOutput,
    ) -> Vec<Event> {
        if !self.phase.calls.iter().any(|call| call.id == call_id) {
            return Vec::new();
        }
        let is_error = output.is_error;
        self.phase.outputs.insert(call_id.clone(), output.clone());
        record(
            context,
            vec![
                Event::ToolExecutionEnd {
                    step: self.phase.number,
                    call_id: call_id.clone(),
                    is_error,
                },
                Event::Message(Message::ToolResult { call_id, output }),
            ],
        )
    }

    /// 是否所有派发过的调用都有了结果。
    pub fn results_complete(&self) -> bool {
        self.phase.outputs.len() == self.phase.calls.len()
    }

    /// 收尾：`step_end{ToolUse}`。只在全部结果落位后调用。
    pub fn settle(self, context: &mut Context) -> Vec<Event> {
        debug_assert!(self.phase.outputs.len() == self.phase.calls.len());
        record(
            context,
            vec![Event::StepEnd {
                step: self.phase.number,
                outcome: StepOutcome::ToolUse,
            }],
        )
    }

    /// 用户打断：未完成调用的结果以合成错误补齐（事件照发），
    /// 再按调用顺序记录结果消息，`step_end{Interrupted}`。
    pub fn interrupted(self, context: &mut Context) -> Vec<Event> {
        let number = self.phase.number;
        let mut events = Vec::new();
        for call in self.phase.calls.iter() {
            if self.phase.outputs.contains_key(&call.id) {
                continue;
            }
            events.push(Event::ToolExecutionEnd {
                step: number,
                call_id: call.id.clone(),
                is_error: true,
            });
            events.push(Event::Message(Message::ToolResult {
                call_id: call.id.clone(),
                output: ToolOutput::error("not run: the user interrupted the agent"),
            }));
        }
        events.push(Event::StepEnd {
            step: number,
            outcome: StepOutcome::Interrupted,
        });
        record(context, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemPrompt;
    use crate::context::Environment;
    use crate::llm::StopReason;
    use crate::message::{MessageDelta, TokenUsage, ToolArguments};
    use crate::tool::ToolName;
    use agnes_util::NonEmpty;
    use std::str::FromStr;

    fn fresh() -> Context {
        Context::new(SystemPrompt::new("sys"), Environment::default())
    }

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    fn usage() -> TokenUsage {
        TokenUsage {
            input: 3,
            output: 5,
        }
    }

    fn response(stop_reason: StopReason) -> LlmResponse {
        LlmResponse {
            stop_reason,
            usage: usage(),
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(id),
            name: ToolName::from_str("note").unwrap(),
            arguments: ToolArguments::new("{}"),
        }
    }

    fn tool_use(calls: Vec<ToolCall>) -> StopReason {
        StopReason::ToolUse {
            content: None,
            calls: NonEmpty::from_vec(calls).unwrap(),
        }
    }

    fn messages(context: &Context) -> Vec<&Message> {
        context.messages().collect()
    }

    /// 事件种类，按顺序 —— 顺序保证就断言在这里。
    fn shape(events: &[Event]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                Event::TurnStart => "turn_start",
                Event::StepStart { .. } => "step_start",
                Event::MessageStart { .. } => "message_start",
                Event::MessageUpdate { .. } => "message_update",
                Event::Message(_) => "message",
                Event::MessageEnd { .. } => "message_end",
                Event::ToolExecutionStart { .. } => "tool_start",
                Event::ToolExecutionEnd { .. } => "tool_end",
                Event::StepEnd { .. } => "step_end",
                Event::UserMessagesInjected { .. } => "user_injected",
                Event::TurnEnd { .. } => "turn_end",
            })
            .collect()
    }

    #[test]
    fn begin_opens_the_step_and_the_message() {
        let mut context = fresh();
        let (step, events) = Step::begin(StepNumber::FIRST, &mut context);

        assert_eq!(step.number(), StepNumber::FIRST);
        assert_eq!(shape(&events), vec!["step_start", "message_start"]);
        assert_eq!(context.events(), events);
        assert!(messages(&context).is_empty());
    }

    #[test]
    fn update_logs_the_delta_without_touching_the_messages() {
        let mut context = fresh();
        let (step, _) = Step::begin(StepNumber::FIRST, &mut context);
        let delta = MessageDelta::Text("flip".into());
        let event = step.update(&mut context, delta.clone());

        assert!(matches!(
            &event,
            Event::MessageUpdate { step, delta: d } if *step == StepNumber::FIRST && *d == delta
        ));
        assert_eq!(
            shape(context.events()),
            vec!["step_start", "message_start", "message_update"]
        );
        assert!(messages(&context).is_empty());
    }

    #[test]
    fn tool_calls_move_the_step_into_the_executing_phase() {
        let mut context = fresh();
        let (step, _) = Step::begin(StepNumber::FIRST, &mut context);
        let StepReply::Tools { mut step, events } = step.message_ended(
            &mut context,
            response(tool_use(vec![call("c1"), call("c2")])),
        ) else {
            panic!("expected Tools");
        };

        assert_eq!(step.number(), StepNumber::FIRST);
        assert_eq!(shape(&events), vec!["message", "message_end"]);
        assert!(matches!(
            messages(&context)[0],
            Message::Assistant(AssistantMessage::ToolCalls { calls, .. }) if calls.len() == 2
        ));
        // 派发按模型给出的顺序进行
        assert_eq!(step.take_next().unwrap().id.as_str(), "c1");
        assert_eq!(step.take_next().unwrap().id.as_str(), "c2");
        assert!(step.take_next().is_none());
    }

    #[test]
    fn an_answer_closes_the_step_with_the_reply() {
        let mut context = fresh();
        let (step, _) = Step::begin(StepNumber::FIRST, &mut context);
        let StepReply::Answered { reply, events } = step.message_ended(
            &mut context,
            response(StopReason::Completed {
                content: content("flipped it"),
            }),
        ) else {
            panic!("expected Answered");
        };

        assert_eq!(reply, content("flipped it"));
        assert_eq!(shape(&events), vec!["message", "message_end", "step_end"]);
        assert!(matches!(
            events.last(),
            Some(Event::StepEnd {
                outcome: StepOutcome::Answered { reply },
                ..
            }) if *reply == content("flipped it")
        ));
        assert!(matches!(
            messages(&context)[0],
            Message::Assistant(AssistantMessage::Text(text)) if text.as_str() == "flipped it"
        ));
    }

    #[test]
    fn tool_lifecycle_events_follow_the_real_calls() {
        let mut context = fresh();
        let (step, _) = Step::begin(StepNumber::FIRST, &mut context);
        let StepReply::Tools { mut step, .. } = step.message_ended(
            &mut context,
            response(tool_use(vec![call("c1"), call("c2")])),
        ) else {
            panic!("expected Tools");
        };

        // 未知 id 被忽略
        assert!(
            step.tool_started(&mut context, &ToolCallId::new("nope"))
                .is_none()
        );
        assert!(
            step.tool_finished(
                &mut context,
                ToolCallId::new("nope"),
                ToolOutput::success("x")
            )
            .is_empty()
        );

        let started = step
            .tool_started(&mut context, &ToolCallId::new("c1"))
            .unwrap();
        assert!(matches!(
            &started,
            Event::ToolExecutionStart { step, call }
                if *step == StepNumber::FIRST && call.id.as_str() == "c1"
        ));
        assert!(!step.results_complete());
        let events = step.tool_finished(
            &mut context,
            ToolCallId::new("c1"),
            ToolOutput::error("boom"),
        );
        assert_eq!(shape(&events), vec!["tool_end", "message"]);
        assert_eq!(
            shape(context.events()),
            vec![
                "step_start",
                "message_start",
                "message",
                "message_end",
                "tool_start",
                "tool_end",
                "message"
            ]
        );
        assert!(matches!(
            messages(&context).last(),
            Some(Message::ToolResult { call_id, output })
                if call_id.as_str() == "c1" && output.is_error
        ));

        // 第二个调用完成即落位齐全
        let _ = step.take_next().unwrap();
        let _ = step.tool_finished(
            &mut context,
            ToolCallId::new("c2"),
            ToolOutput::success("ok"),
        );
        assert!(step.results_complete());
        let events = step.settle(&mut context);
        assert_eq!(shape(&events), vec!["step_end"]);
    }

    #[test]
    fn interrupting_while_generating_records_nothing_but_the_close() {
        let mut context = fresh();
        let (step, _) = Step::begin(StepNumber::FIRST, &mut context);
        let events = step.interrupted(&mut context);

        assert_eq!(shape(&events), vec!["step_end"]);
        assert_eq!(
            shape(context.events()),
            vec!["step_start", "message_start", "step_end"]
        );
        assert!(messages(&context).is_empty());
    }

    #[test]
    fn interrupting_while_executing_fills_synthetics_for_open_calls() {
        let mut context = fresh();
        let (step, _) = Step::begin(StepNumber::FIRST, &mut context);
        let StepReply::Tools { mut step, .. } = step.message_ended(
            &mut context,
            response(tool_use(vec![call("c1"), call("c2")])),
        ) else {
            panic!("expected Tools");
        };
        let first = step.take_next().unwrap();
        assert_eq!(first.id.as_str(), "c1");
        let _ = step.tool_finished(
            &mut context,
            ToolCallId::new("c1"),
            ToolOutput::success("done"),
        );

        assert!(!step.results_complete());
        let events = step.interrupted(&mut context);

        // 只有未完成的 c2 被补上合成结果；c1 的结果保持原样
        assert_eq!(shape(&events), vec!["tool_end", "message", "step_end"]);
        assert!(matches!(
            events.first(),
            Some(Event::ToolExecutionEnd { call_id, is_error: true, .. })
                if call_id.as_str() == "c2"
        ));
        let messages = messages(&context);
        assert!(matches!(
            messages[messages.len() - 2],
            Message::ToolResult { output, .. } if !output.is_error
        ));
        assert!(matches!(
            messages[messages.len() - 1],
            Message::ToolResult { output, .. }
                if output.is_error && output.content.contains("interrupted")
        ));
        assert!(matches!(
            events.last(),
            Some(Event::StepEnd {
                outcome: StepOutcome::Interrupted,
                ..
            })
        ));
    }

    #[test]
    fn length_exceeded_keeps_whatever_the_model_managed() {
        let mut context = fresh();
        let (step, _) = Step::begin(StepNumber::FIRST, &mut context);
        let StepReply::LengthExceeded { events } = step.message_ended(
            &mut context,
            response(StopReason::LengthExceeded {
                content: Some(content("half a thought")),
            }),
        ) else {
            panic!("expected LengthExceeded");
        };
        assert_eq!(shape(&events), vec!["message", "message_end", "step_end"]);
        assert_eq!(messages(&context).len(), 1);

        let mut context = fresh();
        let (step, _) = Step::begin(StepNumber::FIRST, &mut context);
        let StepReply::LengthExceeded { events } = step.message_ended(
            &mut context,
            response(StopReason::LengthExceeded { content: None }),
        ) else {
            panic!("expected LengthExceeded");
        };
        assert_eq!(shape(&events), vec!["message_end", "step_end"]);
        assert!(messages(&context).is_empty());
    }

    #[test]
    fn step_numbers_count_from_one_and_stop_at_the_limit() {
        assert_eq!(StepNumber::FIRST.get(), 1);
        assert_eq!(StepNumber::FIRST.next().unwrap().get(), 2);
        assert_eq!(StepNumber::FIRST.to_string(), "1");
        assert!(StepNumber(NonZeroU32::MAX).next().is_none());
    }
}

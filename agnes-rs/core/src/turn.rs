//! turn：把一次用户请求推进到答复的完整运行 —— 由多个 step（模型往返）组成。
//!
//! 这是事件循环驱动的 sans-io 状态机：外层驱动只执行 IO —— 从
//! [`Turn::poll_actions`] 从 FIFO 动作队列里逐个取走 [`Action`]（报告事件、
//! 把结果作为 [`Input`] 从 [`Turn::handle`] 喂回来。机器只接收输入、消费
//! 事件、更新状态、产生待执行的动作，从不执行 IO；请求与工具执行的策略
//! 完全由宿主导，机器既不知道模型是什么，也不知道工具怎么运行。
//!
//! 外层按当前阶段（[`Phase`]）分发输入，内层是类型状态的 [`Step`] 子机
//! （见 [`crate::step`]）。与阶段不符的输入被原样放回并忽略 —— 状态不丢，
//! 协议纪律由驱动器遵守。
//!
//! 所有发生的事都写进 [`Context`] 的有序日志（消息也是事件）；插话在
//! step 边界生效；turn 在最终答复、打断或 step 预算耗尽时收场，其
//! [`Outcome`] 就是整个 run 的结果。

use crate::config::StepBudget;
use crate::content::Content;
use crate::context::Context;
use crate::event::Event;
use crate::llm::LlmResponse;
use crate::message::{Message, MessageDelta, TokenUsage, ToolCall, ToolCallId, ToolOutput};
use crate::step::{Executing, Generating, Step, StepNumber, StepReply};
use std::collections::VecDeque;
use std::mem;
use std::num::NonZeroU32;

/// 把一个 turn 带起来的全部输入：会话上下文（系统提示、环境）、
/// 开局的用户消息，以及最多可以启动多少个模型往返。
#[derive(Debug)]
pub struct AgentConfig {
    pub context: Context,
    pub goal: Content,
    pub budget: StepBudget,
}

/// 外层喂给机器的输入 —— IO 的结果，或用户动作。
#[derive(Debug, Clone)]
pub enum Input {
    /// 模型消息的一段流式增量（流式适配器才有）。
    MessageDelta(MessageDelta),
    /// 一次模型往返的响应到达。
    ModelResponded(LlmResponse),
    /// 宿主开始执行已派发的工具调用。
    ToolStarted(ToolCallId),
    /// 宿主执行完一个工具调用。
    ToolFinished {
        call_id: ToolCallId,
        output: ToolOutput,
    },
    /// 用户在运行中插话；在下一个 step 边界生效。
    UserMessage(Content),
    /// 用户打断；两个等待阶段都可以打断。
    Interrupted,
}

/// 机器交给外层执行的动作 —— 机器只产生动作，从不执行 IO。
#[derive(Debug)]
pub enum Action {
    /// 把事件上报给界面/日志（机器已经把它写进 Context 日志）。
    Report(Event),
    /// 发起一次模型调用：宿主的 RequestBuilder 从 Context 构造出请求。
    CallModel,
    /// 执行一个工具调用；产生的 `Input::ToolStarted` 与结果要喂回机器。
    /// 一次只派发一个，按模型给出的顺序。
    RunTool(ToolCall),
}

/// turn 怎么收场 —— 也是整个 run 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 模型给出最终答复。
    Answered { reply: Content },
    /// 模型输出触顶。
    LengthExceeded,
    /// 用户打断；日志留着收场前的每一步。
    Interrupted,
    /// step 预算耗尽，不再有下一个模型往返。
    BudgetExhausted,
}

/// 机器的阶段：等消息、等工具，或已收场。
#[derive(Debug)]
enum Phase {
    AwaitingMessage(Step<Generating>),
    AwaitingTools(Step<Executing>),
    Closed(Outcome),
}

/// 事件循环驱动的 sans-io turn 状态机。
#[derive(Debug)]
pub struct Turn {
    context: Context,
    budget: StepBudget,
    usage: TokenUsage,
    steps_started: u32,
    buffered_user_messages: Vec<Content>,
    /// 待执行的动作队列（FIFO），`poll_actions` 每次取走一个。
    actions: VecDeque<Action>,
    phase: Phase,
}

impl Turn {
    /// 开始 turn：goal 先入账（用户先说话），随后 turn_start、第一个 step
    /// 的启动动作 —— 外层从动作里见到 `CallModel` 就发起首次调用。
    pub fn start(config: AgentConfig) -> Self {
        let AgentConfig {
            mut context,
            goal,
            budget,
        } = config;
        let mut actions = VecDeque::new();
        emit(
            &mut context,
            &mut actions,
            Event::Message(Message::User { content: goal }),
        );
        emit(&mut context, &mut actions, Event::TurnStart);
        let (step, events) = Step::begin(StepNumber::FIRST, &mut context);
        for event in events {
            actions.push_back(Action::Report(event));
        }
        actions.push_back(Action::CallModel);
        Self {
            context,
            budget,
            usage: TokenUsage::default(),
            steps_started: 1,
            buffered_user_messages: Vec::new(),
            actions,
            phase: Phase::AwaitingMessage(step),
        }
    }

    /// 消费一个输入，推进状态并把新动作入队。与当前阶段不符的输入被
    /// 原样放回并忽略 —— 状态不丢；已收场的 turn 忽略一切输入。
    pub fn handle(&mut self, input: Input) {
        let Turn {
            context,
            budget,
            usage,
            steps_started,
            buffered_user_messages,
            actions,
            phase,
        } = self;

        match input {
            Input::UserMessage(content) => buffered_user_messages.push(content),
            Input::MessageDelta(delta) => {
                if let Phase::AwaitingMessage(step) = phase {
                    let event = step.update(context, delta);
                    actions.push_back(Action::Report(event));
                }
            }
            Input::ToolStarted(call_id) => {
                if let Phase::AwaitingTools(step) = phase {
                    if let Some(event) = step.tool_started(context, &call_id) {
                        actions.push_back(Action::Report(event));
                    }
                }
            }
            Input::ModelResponded(response) => {
                *usage = usage.saturating_add(response.usage);
                let taken = take_phase(phase);
                match taken {
                    Phase::AwaitingMessage(step) => match step.message_ended(context, response) {
                        StepReply::Tools { mut step, events } => {
                            for event in events {
                                actions.push_back(Action::Report(event));
                            }
                            // 一次只派发一个；下一个等结果喂回后再排队。
                            if let Some(call) = step.take_next() {
                                actions.push_back(Action::RunTool(call));
                            }
                            *phase = Phase::AwaitingTools(step);
                        }
                        StepReply::Answered { reply, events } => {
                            for event in events {
                                actions.push_back(Action::Report(event));
                            }
                            emit(
                                context,
                                actions,
                                Event::TurnEnd {
                                    outcome: Outcome::Answered {
                                        reply: reply.clone(),
                                    },
                                },
                            );
                            *phase = Phase::Closed(Outcome::Answered { reply });
                        }
                        StepReply::LengthExceeded { events } => {
                            for event in events {
                                actions.push_back(Action::Report(event));
                            }
                            emit(
                                context,
                                actions,
                                Event::TurnEnd {
                                    outcome: Outcome::LengthExceeded,
                                },
                            );
                            *phase = Phase::Closed(Outcome::LengthExceeded);
                        }
                    },
                    other => *phase = other,
                }
            }
            Input::ToolFinished { call_id, output } => {
                let taken = take_phase(phase);
                match taken {
                    Phase::AwaitingTools(mut step) => {
                        for event in step.tool_finished(context, call_id, output) {
                            actions.push_back(Action::Report(event));
                        }
                        if step.results_complete() {
                            for event in step.settle(context) {
                                actions.push_back(Action::Report(event));
                            }
                            // step 的边界：运行中的插话在这里生效 ——
                            // 注入事件在前，消息紧随其后入账。
                            let messages = mem::take(buffered_user_messages);
                            if !messages.is_empty() {
                                emit(
                                    context,
                                    actions,
                                    Event::UserMessagesInjected {
                                        messages: messages.clone(),
                                    },
                                );
                                for content in messages {
                                    emit(
                                        context,
                                        actions,
                                        Event::Message(Message::User { content }),
                                    );
                                }
                            }
                            begin_step(context, budget, steps_started, actions, phase);
                        } else {
                            if let Some(call) = step.take_next() {
                                actions.push_back(Action::RunTool(call));
                            }
                            *phase = Phase::AwaitingTools(step);
                        }
                    }
                    other => *phase = other,
                }
            }
            Input::Interrupted => {
                let taken = take_phase(phase);
                match taken {
                    Phase::AwaitingMessage(step) => {
                        for event in step.interrupted(context) {
                            actions.push_back(Action::Report(event));
                        }
                        emit(
                            context,
                            actions,
                            Event::TurnEnd {
                                outcome: Outcome::Interrupted,
                            },
                        );
                        *phase = Phase::Closed(Outcome::Interrupted);
                    }
                    Phase::AwaitingTools(step) => {
                        for event in step.interrupted(context) {
                            actions.push_back(Action::Report(event));
                        }
                        emit(
                            context,
                            actions,
                            Event::TurnEnd {
                                outcome: Outcome::Interrupted,
                            },
                        );
                        *phase = Phase::Closed(Outcome::Interrupted);
                    }
                    other => *phase = other,
                }
            }
        }
    }

    /// 从动作队列取走一个待执行动作（FIFO）；队列空了返回 None。
    pub fn poll_actions(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    /// turn 已收场则给出它的结果。
    pub fn outcome(&self) -> Option<&Outcome> {
        match &self.phase {
            Phase::Closed(outcome) => Some(outcome),
            Phase::AwaitingMessage(_) | Phase::AwaitingTools(_) => None,
        }
    }

    pub fn context(&self) -> &Context {
        &self.context
    }

    pub fn usage(&self) -> TokenUsage {
        self.usage
    }

    /// 已启动的 step 数。
    pub fn steps_used(&self) -> u32 {
        self.steps_started
    }

    /// 结束后仍未生效的插话，还给宿主供下一次使用。
    pub fn take_unapplied_user_messages(&mut self) -> Vec<Content> {
        mem::take(&mut self.buffered_user_messages)
    }

    pub fn into_context(self) -> Context {
        self.context
    }
}

/// 记录一个机器自己的事件并排队报告（step 子机自己写日志）。
fn emit(context: &mut Context, actions: &mut VecDeque<Action>, event: Event) {
    context.record(event.clone());
    actions.push_back(Action::Report(event));
}

/// 开启下一个 step；step 预算不够则 turn 直接收场。
fn begin_step(
    context: &mut Context,
    budget: &StepBudget,
    steps_started: &mut u32,
    actions: &mut VecDeque<Action>,
    phase: &mut Phase,
) {
    if !budget.allows(*steps_started) {
        emit(
            context,
            actions,
            Event::TurnEnd {
                outcome: Outcome::BudgetExhausted,
            },
        );
        *phase = Phase::Closed(Outcome::BudgetExhausted);
        return;
    }
    // steps_started < 预算 ≤ u32::MAX，这里加一不会溢出。
    let number =
        StepNumber::new(NonZeroU32::new(*steps_started + 1).expect("step count fits a u32"));
    *steps_started = number.get();
    let (step, events) = Step::begin(number, context);
    for event in events {
        actions.push_back(Action::Report(event));
    }
    actions.push_back(Action::CallModel);
    *phase = Phase::AwaitingMessage(step);
}

/// 把 phase 取出来以便完成改阶段的转移；占位结果在同一臂内会被立即
/// 覆盖，或在阶段不匹配时原样放回。
fn take_phase(phase: &mut Phase) -> Phase {
    mem::replace(phase, Phase::Closed(Outcome::Interrupted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemPrompt;
    use crate::context::Environment;
    use crate::llm::{Prompt, StopReason};
    use crate::message::{ToolArguments, ToolOutput};
    use crate::tool::ToolName;
    use agnes_util::NonEmpty;
    use std::num::NonZeroU32 as TestNonZeroU32;
    use std::str::FromStr;

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    fn config(budget: u32) -> AgentConfig {
        AgentConfig {
            context: Context::new(SystemPrompt::new("be brief"), Environment::default()),
            goal: content("flip the flag"),
            budget: StepBudget::new(TestNonZeroU32::new(budget).unwrap()),
        }
    }

    fn usage() -> TokenUsage {
        TokenUsage {
            input: 1,
            output: 1,
        }
    }

    fn response(stop_reason: StopReason) -> LlmResponse {
        LlmResponse {
            stop_reason,
            usage: usage(),
        }
    }

    fn completed(text: &str) -> StopReason {
        StopReason::Completed {
            content: content(text),
        }
    }

    fn tool_use(calls: Vec<ToolCall>) -> StopReason {
        StopReason::ToolUse {
            content: None,
            calls: NonEmpty::from_vec(calls).unwrap(),
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(id),
            name: ToolName::from_str("flag").unwrap(),
            arguments: ToolArguments::new("{}"),
        }
    }

    fn output() -> ToolOutput {
        ToolOutput::success("done")
    }

    /// 事件种类，按顺序 —— 顺序保证直接断言在这里。
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

    /// 把动作队列摘成 (报告事件的形状, 模型调用数, 工具调用 id 列表)。
    fn drain(turn: &mut Turn) -> (Vec<&'static str>, usize, Vec<String>) {
        let mut shapes = Vec::new();
        let mut calls = 0;
        let mut tools = Vec::new();
        for action in std::iter::from_fn(|| turn.poll_actions()) {
            match action {
                Action::Report(event) => shapes.extend(shape(std::slice::from_ref(&event))),
                Action::CallModel => calls += 1,
                Action::RunTool(tool_call) => tools.push(tool_call.id.as_str().to_owned()),
            }
        }
        (shapes, calls, tools)
    }

    #[test]
    fn start_reports_the_goal_and_queues_the_first_model_call() {
        let mut turn = Turn::start(config(4));
        let (shapes, calls, tools) = drain(&mut turn);

        assert_eq!(
            shapes,
            vec!["message", "turn_start", "step_start", "message_start"]
        );
        assert_eq!(calls, 1);
        assert!(tools.is_empty());
        assert!(turn.outcome().is_none());
        assert_eq!(turn.steps_used(), 1);

        let prompt = Prompt::from_context(turn.context());
        assert_eq!(prompt.input.len(), 2);
        assert!(matches!(prompt.input[0], Message::System { .. }));
        assert!(
            matches!(&prompt.input[1], Message::User { content } if content.as_str() == "flip the flag")
        );
    }

    #[test]
    fn a_tool_use_response_dispatches_one_call_at_a_time() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);

        turn.handle(Input::ModelResponded(response(tool_use(vec![
            call("c1"),
            call("c2"),
        ]))));
        let (shapes, calls, tools) = drain(&mut turn);
        assert_eq!(shapes, vec!["message", "message_end"]);
        assert_eq!(calls, 0);
        assert_eq!(tools, vec!["c1"]);

        // 第一个结果落位之前，第二个调用不派发
        turn.handle(Input::ToolStarted(ToolCallId::new("c1")));
        turn.handle(Input::ToolFinished {
            call_id: ToolCallId::new("c1"),
            output: output(),
        });
        let (shapes, _calls, tools) = drain(&mut turn);
        assert_eq!(shapes, vec!["tool_start", "tool_end", "message"]);
        assert_eq!(tools, vec!["c2"]);
        assert!(turn.outcome().is_none());
    }

    #[test]
    fn results_settle_the_step_and_start_the_next_one() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        turn.handle(Input::ModelResponded(response(tool_use(vec![call("c1")]))));
        let _ = drain(&mut turn);

        turn.handle(Input::ToolStarted(ToolCallId::new("c1")));
        turn.handle(Input::ToolFinished {
            call_id: ToolCallId::new("c1"),
            output: output(),
        });
        let (shapes, calls, tools) = drain(&mut turn);

        assert_eq!(
            shapes,
            vec![
                "tool_start",
                "tool_end",
                "message",
                "step_end",
                "step_start",
                "message_start"
            ]
        );
        assert_eq!(calls, 1);
        assert!(tools.is_empty());
        assert_eq!(turn.steps_used(), 2);
        assert!(turn.outcome().is_none());
    }

    #[test]
    fn an_answer_ends_the_turn() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);

        turn.handle(Input::ModelResponded(response(completed("done it"))));
        let (shapes, calls, tools) = drain(&mut turn);
        assert_eq!(
            shapes,
            vec!["message", "message_end", "step_end", "turn_end"]
        );
        assert_eq!(calls, 0);
        assert!(tools.is_empty());
        assert_eq!(
            turn.outcome(),
            Some(&Outcome::Answered {
                reply: content("done it")
            })
        );
        // 收场后的输入被忽略
        turn.handle(Input::ModelResponded(response(completed("again"))));
        assert!(drain(&mut turn).0.is_empty());
    }

    #[test]
    fn the_budget_ends_the_turn_before_a_next_step() {
        let mut turn = Turn::start(config(1));
        let _ = drain(&mut turn);

        turn.handle(Input::ModelResponded(response(tool_use(vec![call("c1")]))));
        let _ = drain(&mut turn);
        turn.handle(Input::ToolStarted(ToolCallId::new("c1")));
        turn.handle(Input::ToolFinished {
            call_id: ToolCallId::new("c1"),
            output: output(),
        });

        let (shapes, calls, _tools) = drain(&mut turn);
        assert_eq!(
            shapes,
            vec!["tool_start", "tool_end", "message", "step_end", "turn_end"]
        );
        assert_eq!(calls, 0);
        assert_eq!(turn.outcome(), Some(&Outcome::BudgetExhausted));
        assert_eq!(turn.steps_used(), 1);
    }

    #[test]
    fn user_messages_take_effect_at_the_step_boundary() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        turn.handle(Input::ModelResponded(response(tool_use(vec![call("c1")]))));
        let _ = drain(&mut turn);

        turn.handle(Input::UserMessage(content("also check the tests")));
        // 先入缓存，不动状态
        assert!(drain(&mut turn).0.is_empty());

        turn.handle(Input::ToolStarted(ToolCallId::new("c1")));
        turn.handle(Input::ToolFinished {
            call_id: ToolCallId::new("c1"),
            output: output(),
        });
        let (shapes, _calls, _tools) = drain(&mut turn);
        assert_eq!(
            shapes,
            vec![
                "tool_start",
                "tool_end",
                "message",
                "step_end",
                "user_injected",
                "message",
                "step_start",
                "message_start"
            ]
        );

        let prompt = Prompt::from_context(turn.context());
        // system, goal, assistant(tool calls), tool result, injected user
        assert_eq!(prompt.input.len(), 5);
        assert!(matches!(&prompt.input[3], Message::ToolResult { .. }));
        assert!(matches!(
            &prompt.input[4],
            Message::User { content } if content.as_str() == "also check the tests"
        ));
        assert!(turn.take_unapplied_user_messages().is_empty());
    }

    #[test]
    fn interrupting_the_model_wait_closes_the_turn_cleanly() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);

        turn.handle(Input::Interrupted);
        let (shapes, calls, _tools) = drain(&mut turn);
        assert_eq!(shapes, vec!["step_end", "turn_end"]);
        assert_eq!(calls, 0);
        assert_eq!(turn.outcome(), Some(&Outcome::Interrupted));
        // 被丢弃的响应从未入账
        assert_eq!(turn.context().messages().count(), 1);
    }

    #[test]
    fn interrupting_the_tools_fills_synthetic_results() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        turn.handle(Input::ModelResponded(response(tool_use(vec![call("c1")]))));
        let _ = drain(&mut turn);
        turn.handle(Input::UserMessage(content("hold on")));
        turn.handle(Input::ToolStarted(ToolCallId::new("c1")));
        turn.handle(Input::Interrupted);
        let (shapes, _calls, _tools) = drain(&mut turn);

        assert_eq!(
            shapes,
            vec!["tool_start", "tool_end", "message", "step_end", "turn_end"]
        );
        assert_eq!(turn.outcome(), Some(&Outcome::Interrupted));
        // 边界没到：插话不入日志，随 run 还给宿主
        assert_eq!(
            turn.take_unapplied_user_messages(),
            vec![content("hold on")]
        );
    }

    #[test]
    fn stale_inputs_are_ignored_without_losing_state() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        turn.handle(Input::ModelResponded(response(tool_use(vec![call("c1")]))));
        let _ = drain(&mut turn);
        // 等工具的 step 不接增量，也不接响应
        turn.handle(Input::MessageDelta(MessageDelta::Text("x".into())));
        turn.handle(Input::ModelResponded(response(completed("stale"))));
        assert!(drain(&mut turn).0.is_empty());
        assert!(turn.outcome().is_none());

        // 状态完好：turn 继续走完
        turn.handle(Input::ToolStarted(ToolCallId::new("c1")));
        turn.handle(Input::ToolFinished {
            call_id: ToolCallId::new("c1"),
            output: output(),
        });
        let _ = drain(&mut turn);
        turn.handle(Input::ModelResponded(response(completed("done"))));
        assert_eq!(
            turn.outcome(),
            Some(&Outcome::Answered {
                reply: content("done")
            })
        );
    }

    #[test]
    fn deltas_only_count_while_a_step_generates() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        turn.handle(Input::MessageDelta(MessageDelta::Text("flip".into())));
        let (shapes, _calls, _tools) = drain(&mut turn);
        assert_eq!(shapes, vec!["message_update"]);
    }

    #[test]
    fn usage_and_steps_accumulate_across_steps() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        turn.handle(Input::ModelResponded(response(tool_use(vec![call("c1")]))));
        let _ = drain(&mut turn);
        turn.handle(Input::ToolStarted(ToolCallId::new("c1")));
        turn.handle(Input::ToolFinished {
            call_id: ToolCallId::new("c1"),
            output: output(),
        });
        let _ = drain(&mut turn);
        turn.handle(Input::ModelResponded(response(completed("done"))));

        assert_eq!(
            turn.usage(),
            TokenUsage {
                input: 2,
                output: 2
            }
        );
        assert_eq!(turn.steps_used(), 2);
    }

    #[test]
    fn unapplied_user_messages_come_back_after_an_answer() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        turn.handle(Input::UserMessage(content("too late")));
        turn.handle(Input::ModelResponded(response(completed("done"))));
        let _ = drain(&mut turn);

        assert!(matches!(turn.outcome(), Some(Outcome::Answered { .. })));
        // 最终答复走完、边界未到 —— 插话不属于这次 turn 的日志，还给宿主
        assert_eq!(
            turn.take_unapplied_user_messages(),
            vec![content("too late")]
        );
        assert_eq!(turn.context().messages().count(), 2);
    }

    #[test]
    fn a_full_tool_round_reports_its_lifecycle_in_order() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        turn.handle(Input::ModelResponded(response(tool_use(vec![call("c1")]))));
        let _ = drain(&mut turn);
        turn.handle(Input::ToolStarted(ToolCallId::new("c1")));
        turn.handle(Input::ToolFinished {
            call_id: ToolCallId::new("c1"),
            output: output(),
        });
        let _ = drain(&mut turn);
        turn.handle(Input::ModelResponded(response(completed("done it"))));

        assert_eq!(
            shape(turn.context().events()),
            vec![
                "message",
                "turn_start",
                "step_start",
                "message_start",
                "message",
                "message_end",
                "tool_start",
                "tool_end",
                "message",
                "step_end",
                "step_start",
                "message_start",
                "message",
                "message_end",
                "step_end",
                "turn_end"
            ]
        );
        let messages: Vec<&Message> = turn.context().messages().collect();
        assert_eq!(messages.len(), 4);
        assert!(matches!(messages[0], Message::User { .. }));
        assert!(matches!(messages[1], Message::Assistant(_)));
        assert!(matches!(messages[2], Message::ToolResult { .. }));
        assert!(matches!(messages[3], Message::Assistant(_)));
    }
}

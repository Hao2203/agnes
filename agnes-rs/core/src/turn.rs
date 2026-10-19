//! turn：把一次用户请求推进到答复的完整运行 —— 由多个 step（模型往返）组成。
//!
//! 这是事件循环驱动的 sans-io 状态机，且状态即类型：机器的阶段由
//! [`AwaitingMessage`]、[`AwaitingTools`]、[`Closed`] 表示，[`Turn<Phase>`]
//! 每个阶段只提供推进该阶段的方法 —— 在等工具时"喂"响应、在收场后喂
//! 任何输入，编译期都不可能。乱序输入的运行时容忍随之取消：真实驱动器
//! 用 select! 互斥地产生 IO 结果，正确性由类型而非防御保证。
//!
//! 机器只接收输入、消费事件、更新状态、产生待执行的动作，从不执行 IO：
//! 外层驱动从 [`Turn::poll_actions`] 把 FIFO 动作队列里的 [`Action`] 逐个
//! 取走执行 —— 上报事件、上调模型、派发工具 —— 再把结果经对应阶段的方法
//! 喂回来。机器不知道模型是什么，也不知道工具怎么跑；请求与工具执行的
//! 策略（含权限插件的准否）完全由宿主导，core 的词汇不因插件而扩展。
//!
//! 只有推进阶段的信息才进机器：完整响应、工具调用的开始与结果、打断。
//! 流式增量不走这里 —— 适配器把产生中的消息直报宿主界面。发生的一切
//! 都写进 [`Log`] 的有序日志（消息、系统提示与工具环境都是事件）；插话
//! 随时缓存、在下一个 step 边界生效；turn 在最终答复、打断或 step 预算
//! 耗尽时收场，其 [`Outcome`] 就是整个 run 的结果。

use crate::config::StepBudget;
use crate::content::Content;
use crate::event::Event;
use crate::header::Header;
use crate::llm::{LlmResponse, StopReason};
use crate::log::{EventIndex, Log};
use crate::message::{AssistantMessage, Message, TokenUsage, ToolCall, ToolCallId, ToolOutput};
use agnes_util::NonEmpty;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::mem;
use std::num::NonZeroU32;

/// 把一个 turn 带起来的全部输入：已有的会话日志、这一轮生效的模型侧
/// 配置、开局的用户消息，以及最多可以启动多少个模型往返。
#[derive(Debug)]
pub struct AgentConfig {
    pub log: Log,
    pub header: Header,
    pub goal: Content,
    pub budget: StepBudget,
}

/// 机器交给外层执行的动作 —— 机器只产生动作，从不执行 IO。
#[derive(Debug)]
pub enum Action {
    /// 日志追加了一条：位置即游标，事件本体留在 [`Log::events`] 里。
    Recorded(EventIndex),
    /// 发起一次模型调用：宿主的 RequestBuilder 从日志构造出请求。
    CallModel,
    /// 执行一个工具调用；产生的 `tool_started` 与结果要喂回机器。
    /// 一次只派发一个，按模型给出的顺序。
    RunTool(ToolCall),
}

/// turn 怎么收场 —— 也是整个 run 的结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// step 编号（turn 内 1 起始）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// 模型给出答复。收件箱非空且预算允许时 turn 会续跑，否则随之收场。
    Answered { reply: Content },
    /// 模型要工具：turn 开下一个 step 接着走。
    ToolUse,
    /// 模型输出触顶。
    LengthExceeded,
    /// 用户打断。
    Interrupted,
}

/// 共享内核：机器各阶段都携带的会话与统计状态，外加待执行的动作队列。
#[derive(Debug)]
struct Core {
    log: Log,
    budget: StepBudget,
    usage: TokenUsage,
    steps_started: u32,
    buffered_user_messages: Vec<Content>,
    actions: VecDeque<Action>,
}

impl Core {
    /// 记录一批转移事件：入账并排队通知 —— 每个转移经此落账。
    fn note(&mut self, events: Vec<Event>) {
        for event in events {
            let index = self.log.record(event);
            self.actions.push_back(Action::Recorded(index));
        }
    }

    /// 记录一个机器自己的事件并入队通知（等同单条 note）。
    fn emit(&mut self, event: Event) {
        let index = self.log.record(event);
        self.actions.push_back(Action::Recorded(index));
    }
}

/// 阶段：等模型消息 —— 只接受完整响应、打断和插话。
#[derive(Debug)]
pub struct AwaitingMessage {
    step: StepNumber,
}

/// 阶段：等工具结果 —— 调用按模型给出的顺序派发给宿主，结果按 id 落位。
#[derive(Debug)]
pub struct AwaitingTools {
    step: StepNumber,
    calls: NonEmpty<ToolCall>,
    /// 已交给宿主的调用数。
    dispatched: usize,
    outputs: BTreeMap<ToolCallId, ToolOutput>,
}

impl AwaitingTools {
    /// 把下一个调用交给宿主执行；全部派发完则返回 None。
    fn take_next(&mut self) -> Option<ToolCall> {
        let call = self.calls.as_ref().get(self.dispatched)?.clone();
        self.dispatched += 1;
        Some(call)
    }

    /// 是否所有派发过的调用都有了结果。
    fn results_complete(&self) -> bool {
        self.outputs.len() == self.calls.len()
    }
}

/// 阶段：已收场 —— [`Outcome`] 就是整个 run 的结果；不再接受任何输入。
#[derive(Debug)]
pub struct Closed {
    outcome: Outcome,
}

/// 分阶段 typestate 的 turn 机器：`Turn<AwaitingMessage>` →
/// `Turn<AwaitingTools>` →（回到 `Turn<AwaitingMessage>` 或收为
/// `Turn<Closed>`），每一步转移都消耗当前阶段、交还下一个。
#[derive(Debug)]
pub struct Turn<Phase> {
    core: Core,
    phase: Phase,
}

impl<Phase> Turn<Phase> {
    /// 从动作队列取走一个待执行动作（FIFO）；队列空了返回 None。
    pub fn poll_actions(&mut self) -> Option<Action> {
        self.core.actions.pop_front()
    }

    pub fn usage(&self) -> TokenUsage {
        self.core.usage
    }

    /// 已启动的 step 数。
    pub fn steps_used(&self) -> u32 {
        self.core.steps_started
    }

    pub fn log(&self) -> &Log {
        &self.core.log
    }
}

impl Turn<AwaitingMessage> {
    /// 开始 turn：先记录这一轮生效的模型侧配置（header），再让 goal 入账
    /// （用户先说话），随后 turn_start、第一个 step 的开启事件 —— 外层从
    /// 动作里见到 `CallModel` 就发起首次调用。
    pub fn start(config: AgentConfig) -> Self {
        let AgentConfig {
            log,
            header,
            goal,
            budget,
        } = config;
        let mut core = Core {
            log,
            budget,
            usage: TokenUsage::default(),
            steps_started: 0,
            buffered_user_messages: Vec::new(),
            actions: VecDeque::new(),
        };
        core.emit(Event::Header(header));
        core.emit(Event::Message(Message::User { content: goal }));
        core.emit(Event::TurnStart);
        let phase = begin_step(&mut core);
        Self { core, phase }
    }

    /// 运行中插话：先缓存，不动状态；下一个 step 边界生效。
    pub fn user_message(&mut self, content: Content) {
        self.core.buffered_user_messages.push(content);
    }

    /// 一次模型往返的响应到达：用量落账，随后要么转入工具执行，要么
    /// **收件箱里有待处理的插话且预算允许时注入并开下一个 step**，否则
    /// （答复 / 触顶）收场。
    pub fn model_responded(self, response: LlmResponse) -> MessageReplied {
        let Turn {
            mut core,
            phase: AwaitingMessage { step },
        } = self;
        let LlmResponse { stop_reason, usage } = response;
        core.usage = core.usage.saturating_add(usage);
        let stop = stop_reason.clone();
        match stop_reason {
            StopReason::ToolUse { content, calls } => {
                core.note(vec![
                    Event::Message(Message::Assistant(AssistantMessage::ToolCalls {
                        content,
                        calls: calls.clone(),
                    })),
                    Event::MessageEnd { step, stop, usage },
                ]);
                let mut tools = AwaitingTools {
                    step,
                    calls,
                    dispatched: 0,
                    outputs: BTreeMap::new(),
                };
                // 一次只派发一个；下一个等结果喂回后再排队。
                if let Some(call) = tools.take_next() {
                    core.actions.push_back(Action::RunTool(call));
                }
                MessageReplied::Tools {
                    turn: Turn { core, phase: tools },
                }
            }
            StopReason::Completed { content } => {
                let reply = content.clone();
                core.note(vec![
                    Event::Message(Message::Assistant(AssistantMessage::Text(content))),
                    Event::MessageEnd { step, stop, usage },
                    Event::StepEnd {
                        step,
                        outcome: StepOutcome::Answered {
                            reply: reply.clone(),
                        },
                    },
                ]);
                // 收件箱里有话就不收场：注入后在这个 turn 里开下一个 step。
                // 预算不允许时仍以答复收场，插话原样留给宿主。
                if !core.buffered_user_messages.is_empty() && core.budget.allows(core.steps_started)
                {
                    let messages = mem::take(&mut core.buffered_user_messages);
                    core.emit(Event::UserMessagesInjected {
                        messages: messages.clone(),
                    });
                    for content in messages {
                        core.emit(Event::Message(Message::User { content }));
                    }
                    let phase = begin_step(&mut core);
                    return MessageReplied::Continued {
                        turn: Turn { core, phase },
                    };
                }
                core.emit(Event::TurnEnd {
                    outcome: Outcome::Answered {
                        reply: reply.clone(),
                    },
                });
                MessageReplied::Closed {
                    turn: Turn {
                        core,
                        phase: Closed {
                            outcome: Outcome::Answered { reply },
                        },
                    },
                }
            }
            StopReason::LengthExceeded { content } => {
                let mut events = Vec::with_capacity(3);
                if let Some(content) = content {
                    events.push(Event::Message(Message::Assistant(AssistantMessage::Text(
                        content,
                    ))));
                }
                events.push(Event::MessageEnd { step, stop, usage });
                events.push(Event::StepEnd {
                    step,
                    outcome: StepOutcome::LengthExceeded,
                });
                core.note(events);
                core.emit(Event::TurnEnd {
                    outcome: Outcome::LengthExceeded,
                });
                MessageReplied::Closed {
                    turn: Turn {
                        core,
                        phase: Closed {
                            outcome: Outcome::LengthExceeded,
                        },
                    },
                }
            }
        }
    }

    /// 用户打断（响应未达）：`step_end{Interrupted}` + `turn_end`。
    /// 被丢弃的响应从未入账。
    #[must_use]
    pub fn interrupted(self) -> Turn<Closed> {
        let Turn { mut core, phase } = self;
        core.note(vec![Event::StepEnd {
            step: phase.step,
            outcome: StepOutcome::Interrupted,
        }]);
        core.emit(Event::TurnEnd {
            outcome: Outcome::Interrupted,
        });
        Turn {
            core,
            phase: Closed {
                outcome: Outcome::Interrupted,
            },
        }
    }
}

impl Turn<AwaitingTools> {
    /// 运行中插话：先缓存，不动状态；下一个 step 边界生效。
    pub fn user_message(&mut self, content: Content) {
        self.core.buffered_user_messages.push(content);
    }

    /// 宿主开始执行已派发的调用：`tool_execution_start`；
    /// 未知 id 被忽略。
    pub fn tool_started(&mut self, call_id: &ToolCallId) {
        let Some(call) = self
            .phase
            .calls
            .iter()
            .find(|call| call.id == *call_id)
            .cloned()
        else {
            return;
        };
        let step = self.phase.step;
        self.core.emit(Event::ToolExecutionStart { step, call });
    }

    /// 一个结果落位：可能仍在执行（下一个调用已排队），可能结算并启动
    /// 下一个 step（插话在此生效），可能预算耗尽收场。未知 id 被忽略
    /// （原样交还，不加事件）。
    pub fn tool_finished(self, call_id: ToolCallId, output: ToolOutput) -> ToolsAdvanced {
        let Turn {
            mut core,
            mut phase,
        } = self;
        if !phase.calls.iter().any(|call| call.id == call_id) {
            return ToolsAdvanced::Ongoing {
                turn: Turn { core, phase },
            };
        }
        let is_error = output.is_error;
        let step = phase.step;
        phase.outputs.insert(call_id.clone(), output.clone());
        core.note(vec![
            Event::ToolExecutionEnd {
                step,
                call_id: call_id.clone(),
                is_error,
            },
            Event::Message(Message::ToolResult {
                call_id: call_id.clone(),
                output,
            }),
        ]);
        if !phase.results_complete() {
            // 一次只派发一个；下一个等结果喂回后再排队。
            if let Some(call) = phase.take_next() {
                core.actions.push_back(Action::RunTool(call));
            }
            return ToolsAdvanced::Ongoing {
                turn: Turn { core, phase },
            };
        }

        // 全部落位：结算 step。
        core.note(vec![Event::StepEnd {
            step,
            outcome: StepOutcome::ToolUse,
        }]);
        // step 的边界：运行中的插话在这里生效 ——
        // 注入事件在前，消息紧随其后入账。
        let messages = mem::take(&mut core.buffered_user_messages);
        if !messages.is_empty() {
            core.emit(Event::UserMessagesInjected {
                messages: messages.clone(),
            });
            for content in messages {
                core.emit(Event::Message(Message::User { content }));
            }
        }
        if core.budget.allows(core.steps_started) {
            let phase = begin_step(&mut core);
            ToolsAdvanced::NextStep {
                turn: Turn { core, phase },
            }
        } else {
            core.emit(Event::TurnEnd {
                outcome: Outcome::BudgetExhausted,
            });
            let phase = Closed {
                outcome: Outcome::BudgetExhausted,
            };
            ToolsAdvanced::Closed {
                turn: Turn { core, phase },
            }
        }
    }

    /// 用户打断：未完成调用的结果以合成错误补齐（事件照发），再按调用
    /// 顺序记录结果消息，`step_end{Interrupted}` + `turn_end`。
    #[must_use]
    pub fn interrupted(self) -> Turn<Closed> {
        let Turn { mut core, phase } = self;
        let mut events = Vec::new();
        for call in phase.calls.iter() {
            if phase.outputs.contains_key(&call.id) {
                continue;
            }
            events.push(Event::ToolExecutionEnd {
                step: phase.step,
                call_id: call.id.clone(),
                is_error: true,
            });
            events.push(Event::Message(Message::ToolResult {
                call_id: call.id.clone(),
                output: ToolOutput::error("not run: the user interrupted the agent"),
            }));
        }
        events.push(Event::StepEnd {
            step: phase.step,
            outcome: StepOutcome::Interrupted,
        });
        core.note(events);
        core.emit(Event::TurnEnd {
            outcome: Outcome::Interrupted,
        });
        Turn {
            core,
            phase: Closed {
                outcome: Outcome::Interrupted,
            },
        }
    }
}

impl Turn<Closed> {
    /// turn 已收场，给出它的结果。
    pub fn outcome(&self) -> &Outcome {
        &self.phase.outcome
    }

    /// 收场时仍未生效的插话，还给宿主供接续使用。
    pub fn take_unapplied_user_messages(&mut self) -> Vec<Content> {
        mem::take(&mut self.core.buffered_user_messages)
    }

    /// 收场后的会话日志；作为下一个 [`AgentConfig`] 的 `log` 即可延续会话。
    pub fn into_log(self) -> Log {
        self.core.log
    }
}

/// `model_responded` 的落点。
#[derive(Debug)]
#[must_use]
pub enum MessageReplied {
    /// 模型要工具：进入执行阶段（第一个调用已排队派发）。
    Tools { turn: Turn<AwaitingTools> },
    /// 模型给出答复，但收件箱里有待处理的插话：已注入并开了下一个 step。
    Continued { turn: Turn<AwaitingMessage> },
    /// 答复且没有后续输入、输出触顶，或预算不允许再开一个 step：收场。
    Closed { turn: Turn<Closed> },
}

/// `tool_finished` 的落点。
#[derive(Debug)]
#[must_use]
pub enum ToolsAdvanced {
    /// 结果落位但未收齐：下一个调用已排队。
    Ongoing { turn: Turn<AwaitingTools> },
    /// 全部落位并结算：插话已生效，下一个模型往返已排队。
    NextStep { turn: Turn<AwaitingMessage> },
    /// 预算耗尽：turn 收场。
    Closed { turn: Turn<Closed> },
}

/// 开启一个 step：`step_start` + `message_start`，并把模型调用排进动作；
/// 调用前必须先确认预算允许（见 [`StepBudget::allows`]）。
fn begin_step(core: &mut Core) -> AwaitingMessage {
    // steps_started < 预算 ≤ u32::MAX，这里加一不会溢出。
    let number = core.steps_started + 1;
    core.steps_started = number;
    let step = StepNumber::new(NonZeroU32::new(number).expect("step count fits a u32"));
    core.note(vec![
        Event::StepStart { step },
        Event::MessageStart { step },
    ]);
    core.actions.push_back(Action::CallModel);
    AwaitingMessage { step }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemPrompt;
    use crate::header::{Environment, Header};
    use crate::llm::Prompt;
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
            log: Log::default(),
            header: Header::new(SystemPrompt::new("be brief"), Environment::default()),
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
                Event::Header(_) => "header",
                Event::TurnStart => "turn_start",
                Event::StepStart { .. } => "step_start",
                Event::MessageStart { .. } => "message_start",
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
    fn drain<S>(turn: &mut Turn<S>) -> (Vec<&'static str>, usize, Vec<String>) {
        let mut shapes = Vec::new();
        let mut calls = 0;
        let mut tools = Vec::new();
        while let Some(action) = turn.poll_actions() {
            match action {
                Action::Recorded(index) => {
                    let event = turn.log().event(index).expect("通知指向日志内");
                    shapes.extend(shape(std::slice::from_ref(event)));
                }
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
            vec![
                "header",
                "message",
                "turn_start",
                "step_start",
                "message_start"
            ]
        );
        assert_eq!(calls, 1);
        assert!(tools.is_empty());
        assert_eq!(turn.steps_used(), 1);

        let prompt = Prompt::from_log(turn.log());
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

        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };
        let (shapes, calls, tools) = drain(&mut turn);
        assert_eq!(shapes, vec!["message", "message_end"]);
        assert_eq!(calls, 0);
        assert_eq!(tools, vec!["c1"]);

        // 第一个结果落位之前，第二个调用不派发
        turn.tool_started(&ToolCallId::new("c1"));
        let ToolsAdvanced::Ongoing { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected Ongoing");
        };
        let (shapes, _calls, tools) = drain(&mut turn);
        assert_eq!(shapes, vec!["tool_start", "tool_end", "message"]);
        assert_eq!(tools, vec!["c2"]);
    }

    #[test]
    fn results_settle_the_step_and_start_the_next_one() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn);

        turn.tool_started(&ToolCallId::new("c1"));
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
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
    }

    #[test]
    fn an_answer_ends_the_turn() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);

        let MessageReplied::Closed { mut turn } =
            turn.model_responded(response(completed("done it")))
        else {
            panic!("expected Closed");
        };
        let (shapes, calls, tools) = drain(&mut turn);
        assert_eq!(
            shapes,
            vec!["message", "message_end", "step_end", "turn_end"]
        );
        assert_eq!(calls, 0);
        assert!(tools.is_empty());
        assert_eq!(
            turn.outcome(),
            &Outcome::Answered {
                reply: content("done it")
            }
        );
    }

    #[test]
    fn the_budget_ends_the_turn_before_a_next_step() {
        let mut turn = Turn::start(config(1));
        let _ = drain(&mut turn);

        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn);
        turn.tool_started(&ToolCallId::new("c1"));
        let ToolsAdvanced::Closed { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected Closed");
        };

        let (shapes, calls, _tools) = drain(&mut turn);
        assert_eq!(
            shapes,
            vec!["tool_start", "tool_end", "message", "step_end", "turn_end"]
        );
        assert_eq!(calls, 0);
        assert_eq!(turn.outcome(), &Outcome::BudgetExhausted);
        assert_eq!(turn.steps_used(), 1);
    }

    #[test]
    fn user_messages_take_effect_at_the_step_boundary() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn);

        turn.user_message(content("also check the tests"));
        // 先入缓存，不动状态
        assert!(drain(&mut turn).0.is_empty());

        turn.tool_started(&ToolCallId::new("c1"));
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
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

        let prompt = Prompt::from_log(turn.log());
        // system, goal, assistant(tool calls), tool result, injected user
        assert_eq!(prompt.input.len(), 5);
        assert!(matches!(&prompt.input[3], Message::ToolResult { .. }));
        assert!(matches!(
            &prompt.input[4],
            Message::User { content } if content.as_str() == "also check the tests"
        ));
        // 所有插话已生效，缓冲空了 —— 收场后由 take_unapplied 验证
        let MessageReplied::Closed { mut turn } = turn.model_responded(response(completed("ok")))
        else {
            panic!("expected Closed");
        };
        assert!(turn.take_unapplied_user_messages().is_empty());
    }

    #[test]
    fn interrupting_the_model_wait_closes_the_turn_cleanly() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);

        let mut turn = turn.interrupted();
        let (shapes, calls, _tools) = drain(&mut turn);
        assert_eq!(shapes, vec!["step_end", "turn_end"]);
        assert_eq!(calls, 0);
        assert_eq!(turn.outcome(), &Outcome::Interrupted);
        // 被丢弃的响应从未入账
        assert_eq!(turn.log().messages().count(), 1);
    }

    #[test]
    fn interrupting_the_tools_fills_synthetic_results() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn);
        turn.user_message(content("hold on"));
        turn.tool_started(&ToolCallId::new("c1"));
        let mut turn = turn.interrupted();
        let (shapes, _calls, _tools) = drain(&mut turn);

        assert_eq!(
            shapes,
            vec!["tool_start", "tool_end", "message", "step_end", "turn_end"]
        );
        assert_eq!(turn.outcome(), &Outcome::Interrupted);
        // 边界没到：插话不入日志，随 run 还给宿主
        assert_eq!(
            turn.take_unapplied_user_messages(),
            vec![content("hold on")]
        );
    }

    #[test]
    fn unknown_tool_ids_are_ignored_without_disturbing_state() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn);

        turn.tool_started(&ToolCallId::new("nope"));
        let ToolsAdvanced::Ongoing { mut turn } =
            turn.tool_finished(ToolCallId::new("nope"), ToolOutput::success("x"))
        else {
            panic!("expected Ongoing");
        };
        let (shapes, calls, tools) = drain(&mut turn);
        assert!(shapes.is_empty());
        assert_eq!(calls, 0);
        assert!(tools.is_empty());

        // 状态未被扰动：真实结果照常落位
        turn.tool_started(&ToolCallId::new("c1"));
        let ToolsAdvanced::NextStep { turn } = turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
        assert_eq!(shape(turn.log().events()).last(), Some(&"message_start"));
    }

    #[test]
    fn usage_and_steps_accumulate_across_steps() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn);
        turn.tool_started(&ToolCallId::new("c1"));
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
        let _ = drain(&mut turn);
        let MessageReplied::Closed { mut turn } = turn.model_responded(response(completed("done")))
        else {
            panic!("expected Closed");
        };
        let _ = drain(&mut turn);

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
    fn a_queued_message_keeps_the_turn_alive_after_an_answer() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        turn.user_message(content("one more thing"));
        let MessageReplied::Continued { mut turn } =
            turn.model_responded(response(completed("first answer")))
        else {
            panic!("expected Continued");
        };
        let _ = drain(&mut turn);

        // 答复入账后，插话紧接着入账，并开了下一个 step
        assert_eq!(turn.steps_used(), 2);
        assert_eq!(
            shape(turn.log().events()),
            vec![
                "header",
                "message",
                "turn_start",
                "step_start",
                "message_start",
                "message",
                "message_end",
                "step_end",
                "user_injected",
                "message",
                "step_start",
                "message_start",
            ]
        );

        // 下一次模型调用看得见这条插话
        let prompt = Prompt::from_log(turn.log());
        assert!(prompt.input.iter().any(
            |message| matches!(message, Message::User { content } if content.as_str() == "one more thing")
        ));

        // 第二个答复时收件箱已空，正常收场
        let _ = drain(&mut turn);
        let MessageReplied::Closed { mut turn } =
            turn.model_responded(response(completed("second answer")))
        else {
            panic!("expected Closed");
        };
        let _ = drain(&mut turn);
        assert_eq!(
            turn.outcome(),
            &Outcome::Answered {
                reply: content("second answer")
            }
        );
        assert!(turn.take_unapplied_user_messages().is_empty());
    }

    #[test]
    fn a_queued_message_is_returned_when_the_budget_blocks_another_step() {
        // 只有一步预算：答复后没有余量续跑，插话原样还给宿主
        let mut turn = Turn::start(config(1));
        let _ = drain(&mut turn);
        turn.user_message(content("too late"));
        let MessageReplied::Closed { mut turn } = turn.model_responded(response(completed("done")))
        else {
            panic!("expected Closed");
        };
        let _ = drain(&mut turn);

        assert!(matches!(turn.outcome(), Outcome::Answered { .. }));
        assert_eq!(
            turn.take_unapplied_user_messages(),
            vec![content("too late")]
        );
        assert_eq!(turn.log().messages().count(), 2);
    }

    #[test]
    fn a_full_tool_round_reports_its_lifecycle_in_order() {
        let mut turn = Turn::start(config(4));
        let _ = drain(&mut turn);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn);
        turn.tool_started(&ToolCallId::new("c1"));
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
        let _ = drain(&mut turn);
        let MessageReplied::Closed { mut turn } =
            turn.model_responded(response(completed("done it")))
        else {
            panic!("expected Closed");
        };
        let _ = drain(&mut turn);

        assert_eq!(
            shape(turn.log().events()),
            vec![
                "header",
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
        let messages: Vec<&Message> = turn.log().messages().collect();
        assert_eq!(messages.len(), 4);
        assert!(matches!(messages[0], Message::User { .. }));
        assert!(matches!(messages[1], Message::Assistant(_)));
        assert!(matches!(messages[2], Message::ToolResult { .. }));
        assert!(matches!(messages[3], Message::Assistant(_)));
    }

    #[test]
    fn step_numbers_count_from_one_and_stop_at_the_limit() {
        assert_eq!(StepNumber::FIRST.get(), 1);
        assert_eq!(StepNumber::FIRST.next().unwrap().get(), 2);
        assert_eq!(StepNumber::FIRST.to_string(), "1");
        assert!(StepNumber(NonZeroU32::MAX).next().is_none());
    }
}

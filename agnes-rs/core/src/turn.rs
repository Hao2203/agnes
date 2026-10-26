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
//! 只有推进阶段的信息才进机器：完整响应、工具调用的开始与结果、注入与
//! 打断。流式增量不走这里 —— 适配器把产生中的消息直报宿主界面。机器
//! 不持有日志：每次转移把发生的事件作为 [`Action::Record`] 交给下游，由
//! 驱动按发生顺序记账（消息、系统提示与工具环境都是事件）；注入随时
//! 缓存、在下一个 step 边界生效；turn 在最终答复、打断或 step 预算耗尽
//! 时收场，[`Turn::into_result`] 交出整个 run 的结果 —— 包括收场时仍未
//! 生效的注入消息。

use crate::config::StepBudget;
use crate::content::Content;
use crate::event::Event;
use crate::header::Header;
use crate::llm::{LlmResponse, StopKind, StopReason};
use crate::message::{AssistantMessage, Message, TokenUsage, ToolCall, ToolCallId, ToolOutput};
use agnes_util::NonEmpty;
use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::num::NonZeroU32;

/// 把一个 turn 带起来的全部输入：这一轮生效的模型侧配置、开局的目标
/// 消息，以及最多可以启动多少个模型往返。已有会话日志不在这里 —— 它
/// 属于下游，由驱动持有并记录机器产生的事件。
#[derive(Debug)]
pub struct AgentConfig {
    pub header: Header,
    pub goal: Content,
    pub budget: StepBudget,
}

/// 机器交给外层执行的动作 —— 机器只产生动作，从不执行 IO。
#[derive(Debug)]
pub enum Action {
    /// 一条按发生顺序产生的事件：下游记入自己的日志并上报。
    Record(Event),
    /// 发起一次模型调用：宿主的请求转换者从日志构造出请求。
    CallModel,
    /// 执行一个工具调用；产生的 `tool_started` 与结果要喂回机器。
    /// 一次只派发一个，按模型给出的顺序。
    RunTool(ToolCall),
}

/// turn 怎么收场 —— 也是整个 run 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 模型给出最终答复（答复文本属于日志里的消息，不在这里重复）。
    Answered,
    /// 模型输出触顶。
    LengthExceeded,
    /// 提供方的内容过滤器中止了响应。
    ContentFiltered,
    /// 用户打断；日志留着收场前的每一步。
    Interrupted,
    /// step 预算耗尽，不再有下一个模型往返。
    BudgetExhausted,
}

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

impl From<NonZeroU32> for StepNumber {
    fn from(value: NonZeroU32) -> Self {
        Self(value)
    }
}

impl From<StepNumber> for NonZeroU32 {
    fn from(value: StepNumber) -> Self {
        value.0
    }
}

impl fmt::Display for StepNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 一个 step 怎么收尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// 模型给出最终答复。
    Answered,
    /// 模型要求工具，工具结果已全部补齐。
    ToolUse,
    /// 模型因长度限制停下。
    LengthExceeded,
    /// 模型被内容过滤拦下。
    ContentFiltered,
    /// 用户中断。
    Interrupted,
}

/// 共享内核：机器各阶段的统计状态、待注入消息，外加待执行的动作队列。
#[derive(Debug)]
struct Core {
    budget: StepBudget,
    usage: TokenUsage,
    steps_started: u32,
    buffered_messages: Vec<Message>,
    actions: VecDeque<Action>,
}

impl Core {
    /// 把一批转移事件交给下游记账，保持传入顺序。
    fn note(&mut self, events: impl IntoIterator<Item = Event>) {
        for event in events {
            self.actions.push_back(Action::Record(event));
        }
    }

    /// 记录一个机器自己的事件（等同单条 note）。
    fn emit(&mut self, event: Event) {
        self.note([event]);
    }
}

/// 阶段：等模型消息 —— 只接受完整响应、打断和注入。
#[derive(Debug)]
pub struct AwaitingMessage {
    step: StepNumber,
}

/// 一个调用槽位：调用与它的完成标记绑在一起，顺序即模型给出的顺序。
#[derive(Debug, Clone)]
struct CallSlot {
    call: ToolCall,
    finished: bool,
}

/// 阶段：等工具结果 —— 调用按模型给出的顺序派发给宿主，结果按槽位落位。
#[derive(Debug)]
pub struct AwaitingTools {
    step: StepNumber,
    slots: NonEmpty<CallSlot>,
    /// 已交给宿主的槽位数。
    dispatched: usize,
}

impl AwaitingTools {
    /// 把下一个调用交给宿主执行；全部派发完则返回 None。
    fn take_next(&mut self) -> Option<ToolCall> {
        let slot = self.slots.as_ref().get(self.dispatched)?;
        self.dispatched += 1;
        Some(slot.call.clone())
    }

    /// 当前 in-flight 的槽位（刚派发、尚未收到结果的那个）。
    fn in_flight(&self) -> Option<&CallSlot> {
        let index = self.dispatched.checked_sub(1)?;
        self.slots.as_ref().get(index)
    }

    /// 当前 in-flight 的槽位，可变。
    fn in_flight_mut(&mut self) -> Option<&mut CallSlot> {
        let index = self.dispatched.checked_sub(1)?;
        self.slots.make_mut().get_mut(index)
    }

    /// 是否所有调用都有了结果。
    fn results_complete(&self) -> bool {
        self.slots.iter().all(|slot| slot.finished)
    }
}

/// 阶段：已收场 —— 用 [`Turn::into_result`] 取走整个 run 的结果；不再
/// 接受任何输入。
#[derive(Debug)]
pub struct Closed {
    outcome: Outcome,
}

/// turn 收场的结果：怎么收场、用量、走过的 step，以及收场时仍未生效的
/// 注入消息（还给宿主供接续使用）。
#[derive(Debug)]
pub struct TurnResult {
    pub outcome: Outcome,
    pub usage: TokenUsage,
    pub steps_used: u32,
    pub unapplied: Vec<Message>,
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
}

impl Turn<AwaitingMessage> {
    /// 开始 turn：先产生这一轮生效的模型侧配置（header），再把 goal 作为
    /// 消息事件产生（目标先说话），随后 turn_start、第一个 step 的开启
    /// 事件 —— 外层从动作里见到 `CallModel` 就发起首次调用。
    pub fn start(config: AgentConfig) -> Self {
        let AgentConfig {
            header,
            goal,
            budget,
        } = config;
        let mut core = Core {
            budget,
            usage: TokenUsage::default(),
            steps_started: 0,
            buffered_messages: Vec::new(),
            actions: VecDeque::new(),
        };
        core.emit(Event::Header(header));
        core.emit(Event::Message(Message::User { content: goal }));
        core.emit(Event::TurnStart);
        let phase = begin_step(&mut core);
        Self { core, phase }
    }

    /// 运行中注入一条消息：先缓存，不动状态；下一个 step 边界生效。
    /// 消息不限于用户发出 —— 角色由 [`Message`] 本身保留。
    pub fn inject(&mut self, message: Message) {
        self.core.buffered_messages.push(message);
    }

    /// 一次模型往返的响应到达：用量落账，随后要么转入工具执行，要么
    /// **收件箱里有待处理的注入消息且预算允许时注入并开下一个 step**，
    /// 否则（答复 / 触顶）收场。
    pub fn model_responded(self, response: LlmResponse) -> MessageReplied {
        let Turn {
            mut core,
            phase: AwaitingMessage { step },
        } = self;
        let LlmResponse {
            model,
            stop_reason,
            usage,
        } = response;
        core.usage = core.usage.saturating_add(usage);
        let stop = StopKind::from(&stop_reason);
        match stop_reason {
            StopReason::ToolUse { content, calls } => {
                let slots = calls.map(|call| CallSlot {
                    call: call.clone(),
                    finished: false,
                });
                core.note([
                    Event::Message(Message::Assistant(AssistantMessage::ToolCalls {
                        content,
                        calls: calls.clone(),
                    })),
                    Event::MessageEnd {
                        step,
                        model,
                        stop,
                        usage,
                    },
                ]);
                let mut tools = AwaitingTools {
                    step,
                    slots,
                    dispatched: 0,
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
                core.note([
                    Event::Message(Message::Assistant(AssistantMessage::Text(content))),
                    Event::MessageEnd {
                        step,
                        model,
                        stop,
                        usage,
                    },
                    Event::StepEnd {
                        step,
                        outcome: StepOutcome::Answered,
                    },
                ]);
                // 收件箱里有话就不收场：注入后在这个 turn 里开下一个 step。
                // 预算不允许时仍以答复收场，消息原样留给宿主。
                if !core.buffered_messages.is_empty() && core.budget.allows(core.steps_started) {
                    let messages = mem::take(&mut core.buffered_messages);
                    core.emit(Event::MessagesInjected {
                        count: messages.len(),
                    });
                    for message in messages {
                        core.emit(Event::Message(message));
                    }
                    let phase = begin_step(&mut core);
                    return MessageReplied::Continued {
                        turn: Turn { core, phase },
                    };
                }
                core.emit(Event::TurnEnd {
                    outcome: Outcome::Answered,
                });
                MessageReplied::Closed {
                    turn: Turn {
                        core,
                        phase: Closed {
                            outcome: Outcome::Answered,
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
                events.push(Event::MessageEnd {
                    step,
                    model,
                    stop,
                    usage,
                });
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
            StopReason::ContentFiltered { content } => {
                let mut events = Vec::with_capacity(3);
                if let Some(content) = content {
                    events.push(Event::Message(Message::Assistant(AssistantMessage::Text(
                        content,
                    ))));
                }
                events.push(Event::MessageEnd {
                    step,
                    model,
                    stop,
                    usage,
                });
                events.push(Event::StepEnd {
                    step,
                    outcome: StepOutcome::ContentFiltered,
                });
                core.note(events);
                core.emit(Event::TurnEnd {
                    outcome: Outcome::ContentFiltered,
                });
                MessageReplied::Closed {
                    turn: Turn {
                        core,
                        phase: Closed {
                            outcome: Outcome::ContentFiltered,
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
        core.note([Event::StepEnd {
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
    /// 运行中注入一条消息：先缓存，不动状态；下一个 step 边界生效。
    /// 消息不限于用户发出 —— 角色由 [`Message`] 本身保留。
    pub fn inject(&mut self, message: Message) {
        self.core.buffered_messages.push(message);
    }

    /// 宿主开始执行刚派发的调用：`tool_execution_start`。只认当前
    /// in-flight 的槽位；没有在飞调用时是空操作。
    pub fn tool_started(&mut self) {
        let Some(call) = self.phase.in_flight().map(|slot| slot.call.clone()) else {
            return;
        };
        let step = self.phase.step;
        self.core.emit(Event::ToolExecutionStart { step, call });
    }

    /// 一个结果落位：可能仍在执行（下一个调用已排队），可能结算并启动
    /// 下一个 step（注入在此生效），可能预算耗尽收场。
    ///
    /// 只接受当前 in-flight 槽位且 id 匹配的结果；重复结果、未知 id 与
    /// 未派发调用一律幂等忽略，不产生事件，也不污染日志。
    pub fn tool_finished(self, call_id: ToolCallId, output: ToolOutput) -> ToolsAdvanced {
        let Turn {
            mut core,
            mut phase,
        } = self;
        let Some(slot) = phase.in_flight_mut() else {
            return ToolsAdvanced::Ongoing {
                turn: Turn { core, phase },
            };
        };
        if slot.call.id != call_id || slot.finished {
            return ToolsAdvanced::Ongoing {
                turn: Turn { core, phase },
            };
        }
        slot.finished = true;
        let step = phase.step;
        core.note([
            Event::ToolExecutionEnd {
                step,
                call_id: call_id.clone(),
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
        core.note([Event::StepEnd {
            step,
            outcome: StepOutcome::ToolUse,
        }]);
        // step 的边界：运行中的注入在这里生效 ——
        // 注入事件在前，消息紧随其后入账。
        let messages = mem::take(&mut core.buffered_messages);
        if !messages.is_empty() {
            core.emit(Event::MessagesInjected {
                count: messages.len(),
            });
            for message in messages {
                core.emit(Event::Message(message));
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
        let mut events = Vec::with_capacity(phase.slots.len().saturating_mul(2).saturating_add(1));
        for slot in phase.slots.iter() {
            if slot.finished {
                continue;
            }
            events.push(Event::ToolExecutionEnd {
                step: phase.step,
                call_id: slot.call.id.clone(),
            });
            events.push(Event::Message(Message::ToolResult {
                call_id: slot.call.id.clone(),
                output: ToolOutput::not_run(),
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
    /// turn 已收场，借看结果（断言/检查用）。
    pub fn outcome(&self) -> &Outcome {
        &self.phase.outcome
    }

    /// 取走收场结果的所有权：结果、用量、走过的 step，以及收场时仍未
    /// 生效的注入消息，一次交清。
    pub fn into_result(self) -> TurnResult {
        let Turn {
            core,
            phase: Closed { outcome },
        } = self;
        TurnResult {
            outcome,
            usage: core.usage,
            steps_used: core.steps_started,
            unapplied: core.buffered_messages,
        }
    }
}

/// `model_responded` 的落点。
#[derive(Debug)]
#[must_use]
pub enum MessageReplied {
    /// 模型要工具：进入执行阶段（第一个调用已排队派发）。
    Tools { turn: Turn<AwaitingTools> },
    /// 模型给出答复，但收件箱里有待处理的注入消息：已注入并开了下一个 step。
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
    /// 全部落位并结算：注入已生效，下一个模型往返已排队。
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
    core.note([Event::StepStart { step }, Event::MessageStart { step }]);
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
            model: crate::config::ModelName::from_str("test-model").unwrap(),
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
                Event::MessagesInjected { .. } => "messages_injected",
                Event::TurnEnd { .. } => "turn_end",
            })
            .collect()
    }

    /// 把动作队列摘成 (本次事件的形状, 模型调用数, 工具调用 id 列表)，
    /// 同时把事件按顺序收集起来供投影断言。
    fn drain<S>(
        turn: &mut Turn<S>,
        events: &mut Vec<Event>,
    ) -> (Vec<&'static str>, usize, Vec<String>) {
        let mut shapes = Vec::new();
        let mut calls = 0;
        let mut tools = Vec::new();
        while let Some(action) = turn.poll_actions() {
            match action {
                Action::Record(event) => {
                    shapes.extend(shape(std::slice::from_ref(&event)));
                    events.push(event);
                }
                Action::CallModel => calls += 1,
                Action::RunTool(tool_call) => tools.push(tool_call.id.as_str().to_owned()),
            }
        }
        (shapes, calls, tools)
    }

    fn messages_of(events: &[Event]) -> impl Iterator<Item = &Message> {
        events.iter().filter_map(|event| match event {
            Event::Message(message) => Some(message),
            _ => None,
        })
    }

    fn step_starts(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|event| matches!(event, Event::StepStart { .. }))
            .count()
    }

    fn total_usage(events: &[Event]) -> TokenUsage {
        events
            .iter()
            .filter_map(|event| match event {
                Event::MessageEnd { usage, .. } => Some(*usage),
                _ => None,
            })
            .fold(TokenUsage::default(), TokenUsage::saturating_add)
    }

    fn user(text: &str) -> Message {
        Message::User {
            content: content(text),
        }
    }

    #[test]
    fn start_reports_the_goal_and_queues_the_first_model_call() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let (shapes, calls, tools) = drain(&mut turn, &mut events);

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
        assert_eq!(step_starts(&events), 1);

        let prompt = Prompt::from_events(&events);
        assert_eq!(prompt.input.len(), 2);
        assert!(matches!(prompt.input[0], Message::System { .. }));
        assert!(
            matches!(&prompt.input[1], Message::User { content } if content.as_str() == "flip the flag")
        );
    }

    #[test]
    fn a_tool_use_response_dispatches_one_call_at_a_time() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);

        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };
        let (shapes, calls, tools) = drain(&mut turn, &mut events);
        assert_eq!(shapes, vec!["message", "message_end"]);
        assert_eq!(calls, 0);
        assert_eq!(tools, vec!["c1"]);

        // 第一个结果落位之前，第二个调用不派发
        turn.tool_started();
        let ToolsAdvanced::Ongoing { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected Ongoing");
        };
        let (shapes, _calls, tools) = drain(&mut turn, &mut events);
        assert_eq!(shapes, vec!["tool_start", "tool_end", "message"]);
        assert_eq!(tools, vec!["c2"]);
    }

    #[test]
    fn results_settle_the_step_and_start_the_next_one() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn, &mut events);

        turn.tool_started();
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
        let (shapes, calls, tools) = drain(&mut turn, &mut events);

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
        assert_eq!(step_starts(&events), 2);
    }

    #[test]
    fn an_answer_ends_the_turn() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);

        let MessageReplied::Closed { mut turn } =
            turn.model_responded(response(completed("done it")))
        else {
            panic!("expected Closed");
        };
        let (shapes, calls, tools) = drain(&mut turn, &mut events);
        assert_eq!(
            shapes,
            vec!["message", "message_end", "step_end", "turn_end"]
        );
        assert_eq!(calls, 0);
        assert!(tools.is_empty());
        assert_eq!(turn.outcome(), &Outcome::Answered);
        assert!(messages_of(&events).any(
            |message| matches!(message, Message::Assistant(AssistantMessage::Text(content))
                if content.as_str() == "done it")
        ));
    }

    #[test]
    fn the_budget_ends_the_turn_before_a_next_step() {
        let mut turn = Turn::start(config(1));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);

        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn, &mut events);
        turn.tool_started();
        let ToolsAdvanced::Closed { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected Closed");
        };

        let (shapes, calls, _tools) = drain(&mut turn, &mut events);
        assert_eq!(
            shapes,
            vec!["tool_start", "tool_end", "message", "step_end", "turn_end"]
        );
        assert_eq!(calls, 0);
        assert_eq!(turn.outcome(), &Outcome::BudgetExhausted);
        assert_eq!(step_starts(&events), 1);
    }

    #[test]
    fn injected_messages_take_effect_at_the_step_boundary() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn, &mut events);

        turn.inject(user("also check the tests"));
        turn.inject(Message::System {
            content: SystemPrompt::new("injected note"),
        });
        // 先入缓存，不动状态
        assert!(drain(&mut turn, &mut events).0.is_empty());

        turn.tool_started();
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
        let (shapes, _calls, _tools) = drain(&mut turn, &mut events);
        assert_eq!(
            shapes,
            vec![
                "tool_start",
                "tool_end",
                "message",
                "step_end",
                "messages_injected",
                "message",
                "message",
                "step_start",
                "message_start"
            ]
        );

        let prompt = Prompt::from_events(&events);
        // system, goal, assistant(tool calls), tool result, injected user, injected system
        assert_eq!(prompt.input.len(), 6);
        assert!(matches!(&prompt.input[3], Message::ToolResult { .. }));
        assert!(matches!(
            &prompt.input[4],
            Message::User { content } if content.as_str() == "also check the tests"
        ));
        assert!(matches!(
            &prompt.input[5],
            Message::System { content } if content.as_str() == "injected note"
        ));
        // 所有注入已生效，缓冲空了 —— 收场后由 into_result 验证
        let MessageReplied::Closed { turn } = turn.model_responded(response(completed("ok")))
        else {
            panic!("expected Closed");
        };
        assert!(turn.into_result().unapplied.is_empty());
    }

    #[test]
    fn interrupting_the_model_wait_closes_the_turn_cleanly() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);

        let mut turn = turn.interrupted();
        let (shapes, calls, _tools) = drain(&mut turn, &mut events);
        assert_eq!(shapes, vec!["step_end", "turn_end"]);
        assert_eq!(calls, 0);
        assert_eq!(turn.outcome(), &Outcome::Interrupted);
        // 被丢弃的响应从未入账
        assert_eq!(messages_of(&events).count(), 1);
    }

    #[test]
    fn interrupting_the_tools_fills_synthetic_results() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn, &mut events);
        turn.inject(user("hold on"));
        turn.tool_started();
        let mut turn = turn.interrupted();
        let (shapes, _calls, _tools) = drain(&mut turn, &mut events);

        assert_eq!(
            shapes,
            vec!["tool_start", "tool_end", "message", "step_end", "turn_end"]
        );
        // 边界没到：注入不入日志，随 run 还给宿主
        let result = turn.into_result();
        assert_eq!(result.outcome, Outcome::Interrupted);
        assert_eq!(result.unapplied, vec![user("hold on")]);
    }

    #[test]
    fn unknown_tool_ids_are_ignored_without_disturbing_state() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn, &mut events);

        // 未知 id 的结果被忽略，不产生事件
        let ToolsAdvanced::Ongoing { mut turn } =
            turn.tool_finished(ToolCallId::new("nope"), ToolOutput::success("x"))
        else {
            panic!("expected Ongoing");
        };
        let (shapes, calls, tools) = drain(&mut turn, &mut events);
        assert!(shapes.is_empty());
        assert_eq!(calls, 0);
        assert!(tools.is_empty());

        // 状态未被扰动：真实结果照常落位
        turn.tool_started();
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
        let _ = drain(&mut turn, &mut events);
        assert_eq!(shape(&events).last(), Some(&"message_start"));
    }

    #[test]
    fn duplicate_call_ids_still_complete_without_livelock() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("dup"), call("dup")])))
        else {
            panic!("expected Tools");
        };
        let (_, calls, tools) = drain(&mut turn, &mut events);
        assert_eq!(calls, 0);
        assert_eq!(tools, vec!["dup"]);

        // 第一个结果落位后派发第二个同 id 调用
        turn.tool_started();
        let ToolsAdvanced::Ongoing { mut turn } =
            turn.tool_finished(ToolCallId::new("dup"), output())
        else {
            panic!("expected Ongoing");
        };
        let (_, _, tools) = drain(&mut turn, &mut events);
        assert_eq!(tools, vec!["dup"]);

        // 第二个结果按当前槽位落位，turn 不活锁
        turn.tool_started();
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("dup"), output())
        else {
            panic!("expected NextStep");
        };
        let _ = drain(&mut turn, &mut events);
        assert_eq!(shape(&events).last(), Some(&"message_start"));
    }

    #[test]
    fn a_stale_result_for_the_previous_call_is_ignored() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn, &mut events);

        turn.tool_started();
        let ToolsAdvanced::Ongoing { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected Ongoing");
        };
        let _ = drain(&mut turn, &mut events);

        // c1 的重复结果在 c2 在飞时被忽略，不产生第二条结果事件
        let ToolsAdvanced::Ongoing { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected Ongoing");
        };
        let (shapes, calls, tools) = drain(&mut turn, &mut events);
        assert!(shapes.is_empty());
        assert_eq!(calls, 0);
        assert!(tools.is_empty());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::ToolExecutionEnd { .. }))
                .count(),
            1
        );

        turn.tool_started();
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c2"), output())
        else {
            panic!("expected NextStep");
        };
        let _ = drain(&mut turn, &mut events);
    }

    #[test]
    fn usage_and_steps_accumulate_across_steps() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn, &mut events);
        turn.tool_started();
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Closed { mut turn } = turn.model_responded(response(completed("done")))
        else {
            panic!("expected Closed");
        };
        let _ = drain(&mut turn, &mut events);

        assert_eq!(
            total_usage(&events),
            TokenUsage {
                input: 2,
                output: 2
            }
        );
        assert_eq!(step_starts(&events), 2);
    }

    #[test]
    fn a_queued_message_keeps_the_turn_alive_after_an_answer() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        turn.inject(user("one more thing"));
        let MessageReplied::Continued { mut turn } =
            turn.model_responded(response(completed("first answer")))
        else {
            panic!("expected Continued");
        };
        let _ = drain(&mut turn, &mut events);

        // 答复入账后，注入紧接着入账，并开了下一个 step
        assert_eq!(step_starts(&events), 2);
        assert_eq!(
            shape(&events),
            vec![
                "header",
                "message",
                "turn_start",
                "step_start",
                "message_start",
                "message",
                "message_end",
                "step_end",
                "messages_injected",
                "message",
                "step_start",
                "message_start",
            ]
        );

        // 下一次模型调用看得见这条注入
        let prompt = Prompt::from_events(&events);
        assert!(prompt.input.iter().any(
            |message| matches!(message, Message::User { content } if content.as_str() == "one more thing")
        ));

        // 第二个答复时收件箱已空，正常收场
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Closed { mut turn } =
            turn.model_responded(response(completed("second answer")))
        else {
            panic!("expected Closed");
        };
        let _ = drain(&mut turn, &mut events);
        let result = turn.into_result();
        assert_eq!(result.outcome, Outcome::Answered);
        assert!(messages_of(&events).any(
            |message| matches!(message, Message::Assistant(AssistantMessage::Text(content))
                if content.as_str() == "second answer")
        ));
        assert!(result.unapplied.is_empty());
    }

    #[test]
    fn a_queued_message_is_returned_when_the_budget_blocks_another_step() {
        // 只有一步预算：答复后没有余量续跑，注入原样还给宿主
        let mut turn = Turn::start(config(1));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        turn.inject(user("too late"));
        let MessageReplied::Closed { mut turn } = turn.model_responded(response(completed("done")))
        else {
            panic!("expected Closed");
        };
        let _ = drain(&mut turn, &mut events);

        let result = turn.into_result();
        assert!(matches!(result.outcome, Outcome::Answered));
        assert_eq!(result.unapplied, vec![user("too late")]);
        assert_eq!(messages_of(&events).count(), 2);
    }

    #[test]
    fn a_full_tool_round_reports_its_lifecycle_in_order() {
        let mut turn = Turn::start(config(4));
        let mut events = Vec::new();
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Tools { mut turn } =
            turn.model_responded(response(tool_use(vec![call("c1")])))
        else {
            panic!("expected Tools");
        };
        let _ = drain(&mut turn, &mut events);
        turn.tool_started();
        let ToolsAdvanced::NextStep { mut turn } =
            turn.tool_finished(ToolCallId::new("c1"), output())
        else {
            panic!("expected NextStep");
        };
        let _ = drain(&mut turn, &mut events);
        let MessageReplied::Closed { mut turn } =
            turn.model_responded(response(completed("done it")))
        else {
            panic!("expected Closed");
        };
        let _ = drain(&mut turn, &mut events);

        assert_eq!(
            shape(&events),
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
        let messages: Vec<&Message> = messages_of(&events).collect();
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

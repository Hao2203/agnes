//! turn：把一次用户请求推进到答复的完整运行 —— 由多个 step（模型往返）组成。
//!
//! 这是事件循环驱动的 sans-io 状态机，且状态即类型：机器的阶段由
//! [`AwaitingModel`]、[`AwaitingTools`]、[`Closed`] 表示，[`Turn<Phase>`]
//! 每个阶段只提供推进该阶段的方法 —— 在等工具时"喂"响应、在收场后喂
//! 任何输入，编译期都不可能。
//!
//! 机器只持有领域状态、从不记录、也从不执行 IO：[`Turn`] 持有一次 run 的
//! [`Context`]（消息、模型侧配置、用量、step 记账、待注入消息），转移只
//! 交还下一个阶段（或其枚举），阶段本身就是动作提示 —— `AwaitingModel`
//! 表示该调模型，`AwaitingTools` 表示该跑这批调用。事件与日志完全属于
//! 下游：需要什么由驱动通过访问器读取并自行记录。
//!
//! 只有推进阶段的信息才进机器：完整响应、工具调用的结果、注入与打断。
//! 流式增量不走这里 —— 适配器把产生中的消息直报宿主界面。注入随时缓存、
//! 在下一个 step 边界生效；turn 在最终答复、打断或 step 预算耗尽时收场，
//! [`Turn::into_result`] 交出整个 run 的结果 —— 包括收场时仍未生效的
//! 注入消息。

use crate::config::StepBudget;
use crate::content::Content;
use crate::context::Context;
use crate::header::{Environment, Header};
use crate::llm::{LlmResponse, Prompt, StopReason};
use crate::message::{AssistantMessage, Message, TokenUsage, ToolCall, ToolCallId, ToolOutput};
use crate::tool::{ToolName, ToolSpec};
use agnes_util::NonEmpty;
use std::fmt;
use std::num::NonZeroU32;

/// 把一个 turn 带起来的全部输入：这一轮生效的模型侧配置、开局的目标
/// 消息，以及最多可以启动多少个模型往返。已有会话历史不在这里 —— 它由
/// 宿主重建进 [`Context`]。
#[derive(Debug)]
pub struct AgentConfig {
    pub header: Header,
    pub goal: Content,
    pub budget: StepBudget,
}

/// turn 怎么收场 —— 也是整个 run 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 模型给出最终答复（答复文本属于上下文里的消息，不在这里重复）。
    Answered,
    /// 模型输出触顶。
    LengthExceeded,
    /// 提供方的内容过滤器中止了响应。
    ContentFiltered,
    /// 用户打断；上下文留着收场前的每一步。
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

/// 阶段：等模型消息 —— 只接受完整响应、打断和注入。
#[derive(Debug)]
pub struct AwaitingModel {
    step: StepNumber,
}

/// 一个调用槽位：只记录调用与是否已落位；结果本身由 [`Context`] 持有。
#[derive(Debug, Clone)]
struct ToolSlot {
    call: ToolCall,
    done: bool,
}

/// 阶段：等工具结果 —— 全部调用一次性交给宿主，结果按 id 落位
/// （顺序不限）。
#[derive(Debug)]
pub struct AwaitingTools {
    step: StepNumber,
    slots: NonEmpty<ToolSlot>,
}

impl AwaitingTools {
    /// 未完成的同名槽位（重复 id 按模型给出的顺序逐个落位）。
    fn slot_mut(&mut self, call_id: &ToolCallId) -> Option<&mut ToolSlot> {
        self.slots
            .make_mut()
            .iter_mut()
            .find(|slot| !slot.done && slot.call.id == *call_id)
    }

    fn all_finished(&self) -> bool {
        self.slots.iter().all(|slot| slot.done)
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
#[must_use]
pub struct TurnResult {
    pub outcome: Outcome,
    pub usage: TokenUsage,
    pub steps_used: u32,
    pub unapplied: Vec<Message>,
}

/// 分阶段 typestate 的 turn 机器：`Turn<AwaitingModel>` →
/// `Turn<AwaitingTools>` →（回到 `Turn<AwaitingModel>` 或收为
/// `Turn<Closed>`），每一步转移都消耗当前阶段、交还下一个。
#[derive(Debug)]
pub struct Turn<Phase> {
    context: Context,
    phase: Phase,
}

impl<Phase> Turn<Phase> {
    /// 本次 run 的领域状态。
    pub fn context(&self) -> &Context {
        &self.context
    }
}

mod sealed {
    pub trait Sealed {}
}

impl sealed::Sealed for AwaitingModel {}
impl sealed::Sealed for AwaitingTools {}

/// 尚未收场的阶段 —— 只有这些阶段可以注入消息、增删工具、替换环境。
pub trait OpenPhase: sealed::Sealed {}

impl OpenPhase for AwaitingModel {}
impl OpenPhase for AwaitingTools {}

impl<P: OpenPhase> Turn<P> {
    /// 未收场阶段的领域状态（可变）；收场后只能通过 [`Turn::into_result`]
    /// 取走结果。
    fn context_mut(&mut self) -> &mut Context {
        &mut self.context
    }

    /// 运行中注入一条消息：先缓存，在下一个 step 边界生效。
    pub fn inject(&mut self, message: Message) {
        self.context_mut().inject(message);
    }

    /// 运行期增加一个工具；重名是 no-op，返回环境是否变化。
    pub fn add_tool(&mut self, spec: ToolSpec) -> bool {
        self.context_mut().add_tool(spec)
    }

    /// 运行期移除一个工具；不存在是 no-op。
    pub fn remove_tool(&mut self, name: &ToolName) -> bool {
        self.context_mut().remove_tool(name)
    }

    /// 整体替换工具环境。
    pub fn set_environment(&mut self, environment: Environment) {
        self.context_mut().set_environment(environment);
    }
}

impl Turn<AwaitingModel> {
    /// 开始 turn：把 goal 追加到上下文，并开启第一个 step。
    pub fn start(mut context: Context, goal: Content) -> Self {
        context.push_message(Message::User { content: goal });
        let step = context.begin_step();
        Self {
            context,
            phase: AwaitingModel { step },
        }
    }

    pub fn step(&self) -> StepNumber {
        self.phase.step
    }

    /// 标准的模型输入投影（system + 消息 + 当前工具表）。
    pub fn prompt(&self) -> Prompt {
        self.context.prompt()
    }

    /// 一次模型往返的响应到达：用量落账，随后要么转入工具执行，要么
    /// **收件箱里有待处理的注入消息且预算允许时注入并开下一个 step**，
    /// 否则（答复 / 触顶）收场。
    pub fn respond(self, response: LlmResponse) -> Replied {
        let Turn { mut context, phase } = self;
        let LlmResponse { stop_reason, usage } = response;
        context.note_usage(usage);
        match stop_reason {
            StopReason::ToolUse { content, calls } => {
                let slots = calls.map(|call| ToolSlot {
                    call: call.clone(),
                    done: false,
                });
                context.push_message(Message::Assistant(AssistantMessage::ToolCalls {
                    content,
                    calls,
                }));
                Replied::Tools(Turn {
                    context,
                    phase: AwaitingTools {
                        step: phase.step,
                        slots,
                    },
                })
            }
            StopReason::Completed { content } => {
                context.push_message(Message::Assistant(AssistantMessage::Text(content)));
                // 收件箱里有话就不收场：注入后在这个 turn 里开下一个 step。
                // 预算不允许时仍以答复收场，消息原样留给宿主。
                if context.has_buffered() && context.budget_allows_next() {
                    context.apply_injections();
                    let step = context.begin_step();
                    Replied::Next(Turn {
                        context,
                        phase: AwaitingModel { step },
                    })
                } else {
                    Replied::Closed(Turn {
                        context,
                        phase: Closed {
                            outcome: Outcome::Answered,
                        },
                    })
                }
            }
            StopReason::LengthExceeded { content } => {
                if let Some(content) = content {
                    context.push_message(Message::Assistant(AssistantMessage::Text(content)));
                }
                Replied::Closed(Turn {
                    context,
                    phase: Closed {
                        outcome: Outcome::LengthExceeded,
                    },
                })
            }
            StopReason::ContentFiltered { content } => {
                if let Some(content) = content {
                    context.push_message(Message::Assistant(AssistantMessage::Text(content)));
                }
                Replied::Closed(Turn {
                    context,
                    phase: Closed {
                        outcome: Outcome::ContentFiltered,
                    },
                })
            }
        }
    }

    /// 用户打断（响应未达）：turn 以 `Interrupted` 收场，上下文里没有
    /// 被丢弃响应的任何痕迹。
    pub fn interrupted(self) -> Turn<Closed> {
        Turn {
            context: self.context,
            phase: Closed {
                outcome: Outcome::Interrupted,
            },
        }
    }
}

impl Turn<AwaitingTools> {
    pub fn step(&self) -> StepNumber {
        self.phase.step
    }

    /// 模型这一轮要求的全部调用，按模型给出的顺序。
    pub fn calls(&self) -> impl Iterator<Item = &ToolCall> + '_ {
        self.phase.slots.iter().map(|slot| &slot.call)
    }

    /// 全部尚未拿到结果的调用，按模型给出的顺序。驱动在打断时据此为
    /// 每个未完成调用补记 `ToolExecutionEnd`。
    pub fn unfinished(&self) -> impl Iterator<Item = &ToolCall> + '_ {
        self.phase
            .slots
            .iter()
            .filter(|slot| !slot.done)
            .map(|slot| &slot.call)
    }

    /// 第一个还没有结果的调用 —— 顺序执行时的下一个目标。
    pub fn next_unfinished(&self) -> Option<&ToolCall> {
        self.unfinished().next()
    }

    /// 一个结果落位：可能仍在等其他调用，可能结算并启动下一个 step
    /// （注入在此生效），可能预算耗尽收场。
    ///
    /// 只接受未完成槽位且 id 匹配的结果，顺序不限。未知 id 与重复结果
    /// 是驱动契约违例：debug 构建用 `debug_assert` 暴露，release 构建
    /// 幂等忽略，不污染上下文。
    pub fn finish(self, call_id: ToolCallId, output: ToolOutput) -> Advanced {
        let Turn {
            mut context,
            mut phase,
        } = self;
        let Some(slot) = phase.slot_mut(&call_id) else {
            debug_assert!(
                false,
                "tool result for an unknown or already-finished call: {}",
                call_id.as_str()
            );
            return Advanced::Ongoing(Turn { context, phase });
        };
        slot.done = true;
        context.push_message(Message::ToolResult { call_id, output });
        if !phase.all_finished() {
            return Advanced::Ongoing(Turn { context, phase });
        }

        // step 的边界：只有确实要开下一个 step 时，注入才生效；预算耗尽
        // 时消息留在缓冲里，由 `into_result` 还给宿主。
        if context.budget_allows_next() {
            if context.has_buffered() {
                context.apply_injections();
            }
            let step = context.begin_step();
            Advanced::Next(Turn {
                context,
                phase: AwaitingModel { step },
            })
        } else {
            Advanced::Closed(Turn {
                context,
                phase: Closed {
                    outcome: Outcome::BudgetExhausted,
                },
            })
        }
    }

    /// 用户打断：未完成调用的结果以合成错误补齐（按调用顺序），turn 以
    /// `Interrupted` 收场。
    pub fn interrupted(self) -> Turn<Closed> {
        let Turn { mut context, phase } = self;
        for slot in phase.slots.iter() {
            if !slot.done {
                context.push_message(Message::ToolResult {
                    call_id: slot.call.id.clone(),
                    output: ToolOutput::not_run(),
                });
            }
        }
        Turn {
            context,
            phase: Closed {
                outcome: Outcome::Interrupted,
            },
        }
    }
}

impl Turn<Closed> {
    /// turn 已收场，借看结果。
    pub fn outcome(&self) -> Outcome {
        self.phase.outcome
    }

    /// 取走收场结果的所有权：结果、用量、走过的 step，以及收场时仍未
    /// 生效的注入消息，一次交清。
    pub fn into_result(self) -> TurnResult {
        let Turn { mut context, phase } = self;
        TurnResult {
            outcome: phase.outcome,
            usage: context.usage(),
            steps_used: context.steps_started(),
            unapplied: context.take_buffered(),
        }
    }
}

/// `respond` 的落点。
#[derive(Debug)]
#[must_use]
pub enum Replied {
    /// 模型要工具：进入执行阶段（全部调用已交给宿主）。
    Tools(Turn<AwaitingTools>),
    /// 模型给出答复，但收件箱里有待处理的注入消息：已注入并开了下一个 step。
    Next(Turn<AwaitingModel>),
    /// 答复且没有后续输入、输出触顶，或预算不允许再开一个 step：收场。
    Closed(Turn<Closed>),
}

/// `finish` 的落点。
#[derive(Debug)]
#[must_use]
pub enum Advanced {
    /// 结果落位但未收齐：继续等其余调用。
    Ongoing(Turn<AwaitingTools>),
    /// 全部落位并结算：注入已生效，下一个模型往返已就绪。
    Next(Turn<AwaitingModel>),
    /// 预算耗尽：turn 收场。
    Closed(Turn<Closed>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemPrompt;
    use crate::header::Environment;
    use crate::message::{ToolArguments, ToolOutput};
    use crate::tool::{JsonSchema, ToolName};
    use std::num::NonZeroU32 as TestNonZeroU32;
    use std::str::FromStr;

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    fn header() -> Header {
        Header::new(SystemPrompt::new("be brief"), Environment::default())
    }

    fn budget(steps: u32) -> StepBudget {
        StepBudget::new(TestNonZeroU32::new(steps).unwrap())
    }

    fn context(steps: u32) -> Context {
        Context::new(header(), budget(steps))
    }

    fn response(stop_reason: StopReason) -> LlmResponse {
        LlmResponse {
            stop_reason,
            usage: TokenUsage {
                input: 1,
                output: 1,
            },
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

    fn user(text: &str) -> Message {
        Message::User {
            content: content(text),
        }
    }

    fn tool_results(context: &Context) -> Vec<String> {
        context
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult { call_id, .. } => Some(call_id.as_str().to_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn start_appends_the_goal_and_opens_the_first_step() {
        let turn = Turn::start(context(4), content("flip the flag"));
        assert_eq!(turn.step().get(), 1);
        assert_eq!(turn.context().messages(), &[user("flip the flag")]);
        assert_eq!(turn.context().steps_started(), 1);

        let prompt = turn.prompt();
        assert_eq!(prompt.input.len(), 2);
        assert!(matches!(prompt.input[0], Message::System { .. }));
        assert!(
            matches!(&prompt.input[1], Message::User { content } if content.as_str() == "flip the flag")
        );
    }

    #[test]
    fn a_tool_use_response_hands_out_every_call_at_once() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Tools(turn) = turn.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };

        let calls: Vec<&str> = turn.calls().map(|call| call.id.as_str()).collect();
        assert_eq!(calls, ["c1", "c2"]);
        assert_eq!(
            turn.next_unfinished().map(|call| call.id.as_str()),
            Some("c1")
        );
        assert!(matches!(
            turn.context().messages().last(),
            Some(Message::Assistant(AssistantMessage::ToolCalls { .. }))
        ));
    }

    #[test]
    fn results_may_arrive_in_any_order() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Tools(turn) = turn.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };

        let Advanced::Ongoing(turn) = turn.finish(ToolCallId::new("c2"), output()) else {
            panic!("expected Ongoing");
        };
        assert_eq!(
            turn.next_unfinished().map(|call| call.id.as_str()),
            Some("c1")
        );

        let Advanced::Next(turn) = turn.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Next");
        };
        assert_eq!(turn.step().get(), 2);
        assert_eq!(tool_results(turn.context()), ["c2", "c1"]);

        // system, goal, assistant(tool calls), result c2, result c1
        assert_eq!(turn.prompt().input.len(), 5);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn unknown_and_duplicate_results_are_ignored_in_release() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Tools(turn) = turn.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };

        let Advanced::Ongoing(turn) = turn.finish(ToolCallId::new("nope"), output()) else {
            panic!("expected Ongoing");
        };
        let Advanced::Ongoing(turn) = turn.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Ongoing");
        };
        // 重复结果不产生第二条结果消息
        let Advanced::Ongoing(turn) = turn.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Ongoing");
        };
        assert_eq!(tool_results(turn.context()), ["c1"]);

        let Advanced::Next(_) = turn.finish(ToolCallId::new("c2"), output()) else {
            panic!("expected Next");
        };
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "unknown or already-finished call")]
    fn an_unknown_result_is_a_driver_contract_violation() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Tools(turn) = turn.respond(response(tool_use(vec![call("c1")]))) else {
            panic!("expected Tools");
        };
        let _ = turn.finish(ToolCallId::new("nope"), output());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "unknown or already-finished call")]
    fn a_duplicate_result_is_a_driver_contract_violation() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Tools(turn) = turn.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };
        let Advanced::Ongoing(turn) = turn.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Ongoing");
        };
        let _ = turn.finish(ToolCallId::new("c1"), output());
    }

    #[test]
    fn duplicate_call_ids_still_complete_without_livelock() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Tools(turn) = turn.respond(response(tool_use(vec![call("dup"), call("dup")])))
        else {
            panic!("expected Tools");
        };
        let Advanced::Ongoing(turn) = turn.finish(ToolCallId::new("dup"), output()) else {
            panic!("expected Ongoing");
        };
        let Advanced::Next(turn) = turn.finish(ToolCallId::new("dup"), output()) else {
            panic!("expected Next");
        };
        assert_eq!(tool_results(turn.context()), ["dup", "dup"]);
    }

    #[test]
    fn an_answer_ends_the_turn() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Closed(turn) = turn.respond(response(completed("done it"))) else {
            panic!("expected Closed");
        };
        assert_eq!(turn.outcome(), Outcome::Answered);
        assert!(matches!(
            turn.context().messages(),
            [Message::User { .. }, Message::Assistant(AssistantMessage::Text(text))]
                if text.as_str() == "done it"
        ));
        let result = turn.into_result();
        assert_eq!(result.steps_used, 1);
        assert!(result.unapplied.is_empty());
    }

    #[test]
    fn a_buffered_message_continues_after_an_answer() {
        let mut turn = Turn::start(context(4), content("flip"));
        turn.inject(user("one more thing"));
        let Replied::Next(turn) = turn.respond(response(completed("first answer"))) else {
            panic!("expected Next");
        };

        assert_eq!(turn.step().get(), 2);
        let prompt = turn.prompt();
        assert!(prompt.input.iter().any(
            |message| matches!(message, Message::User { content } if content.as_str() == "one more thing")
        ));
        assert!(matches!(
            prompt.input.last(),
            Some(Message::User { content }) if content.as_str() == "one more thing"
        ));
    }

    #[test]
    fn a_buffered_message_stays_unapplied_when_the_budget_blocks_another_step() {
        let mut turn = Turn::start(context(1), content("flip"));
        turn.inject(user("too late"));
        let Replied::Closed(turn) = turn.respond(response(completed("done"))) else {
            panic!("expected Closed");
        };
        assert_eq!(turn.outcome(), Outcome::Answered);
        let result = turn.into_result();
        assert_eq!(result.unapplied, vec![user("too late")]);
    }

    #[test]
    fn the_budget_ends_the_turn_after_a_tool_round() {
        let turn = Turn::start(context(1), content("flip"));
        let Replied::Tools(turn) = turn.respond(response(tool_use(vec![call("c1")]))) else {
            panic!("expected Tools");
        };
        let Advanced::Closed(turn) = turn.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Closed");
        };
        assert_eq!(turn.outcome(), Outcome::BudgetExhausted);
    }

    #[test]
    fn a_buffered_message_stays_unapplied_when_the_budget_blocks_the_next_tool_step() {
        let turn = Turn::start(context(1), content("flip"));
        let Replied::Tools(mut turn) = turn.respond(response(tool_use(vec![call("c1")]))) else {
            panic!("expected Tools");
        };
        turn.inject(user("too late"));

        let Advanced::Closed(turn) = turn.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Closed");
        };
        assert_eq!(turn.outcome(), Outcome::BudgetExhausted);
        let result = turn.into_result();
        assert_eq!(result.unapplied, vec![user("too late")]);
    }

    #[test]
    fn injections_take_effect_at_the_tool_boundary() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Tools(mut turn) = turn.respond(response(tool_use(vec![call("c1")]))) else {
            panic!("expected Tools");
        };
        turn.inject(user("also check the tests"));
        assert_eq!(turn.context().buffered().len(), 1);

        let Advanced::Next(turn) = turn.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Next");
        };
        assert!(turn.context().buffered().is_empty());
        let prompt = turn.prompt();
        assert!(matches!(
            prompt.input.last(),
            Some(Message::User { content }) if content.as_str() == "also check the tests"
        ));
    }

    #[test]
    fn length_and_filtered_responses_close_the_turn() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Closed(turn) =
            turn.respond(response(StopReason::LengthExceeded { content: None }))
        else {
            panic!("expected Closed");
        };
        assert_eq!(turn.outcome(), Outcome::LengthExceeded);

        let turn = Turn::start(context(4), content("flip"));
        let Replied::Closed(turn) = turn.respond(response(StopReason::ContentFiltered {
            content: Some(content("partial")),
        })) else {
            panic!("expected Closed");
        };
        assert_eq!(turn.outcome(), Outcome::ContentFiltered);
        assert!(matches!(
            turn.context().messages().last(),
            Some(Message::Assistant(AssistantMessage::Text(text))) if text.as_str() == "partial"
        ));
    }

    #[test]
    fn interrupting_the_model_wait_leaves_injections_unapplied() {
        let mut turn = Turn::start(context(4), content("flip"));
        turn.inject(user("hold on"));
        let turn = turn.interrupted();
        assert_eq!(turn.outcome(), Outcome::Interrupted);
        let result = turn.into_result();
        assert_eq!(result.unapplied, vec![user("hold on")]);
        assert_eq!(result.steps_used, 1);
    }

    #[test]
    fn interrupting_the_tools_fills_synthetic_results() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Tools(turn) = turn.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };
        let Advanced::Ongoing(mut turn) = turn.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Ongoing");
        };
        turn.inject(user("hold on"));

        let turn = turn.interrupted();
        assert_eq!(turn.outcome(), Outcome::Interrupted);
        let messages = turn.context().messages();
        assert!(matches!(
            messages.last(),
            Some(Message::ToolResult { call_id, output })
                if call_id.as_str() == "c2" && output.is_error()
        ));
        let result = turn.into_result();
        assert_eq!(result.unapplied, vec![user("hold on")]);
    }

    #[test]
    fn usage_and_steps_accumulate_across_steps() {
        let turn = Turn::start(context(4), content("flip"));
        let Replied::Tools(turn) = turn.respond(response(tool_use(vec![call("c1")]))) else {
            panic!("expected Tools");
        };
        let Advanced::Next(turn) = turn.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Next");
        };
        let Replied::Closed(turn) = turn.respond(response(completed("done"))) else {
            panic!("expected Closed");
        };

        let result = turn.into_result();
        assert_eq!(
            result.usage,
            TokenUsage {
                input: 2,
                output: 2
            }
        );
        assert_eq!(result.steps_used, 2);
    }

    #[test]
    fn tools_can_be_added_and_removed_while_open() {
        let mut turn = Turn::start(context(4), content("flip"));
        let spec = ToolSpec::new(
            ToolName::from_str("injected").unwrap(),
            "an injected tool",
            JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        );
        assert!(turn.add_tool(spec));
        assert_eq!(turn.prompt().tools.len(), 1);
        assert!(turn.remove_tool(&ToolName::from_str("injected").unwrap()));
        assert!(turn.prompt().tools.is_empty());
    }

    #[test]
    fn step_numbers_count_from_one_and_stop_at_the_limit() {
        assert_eq!(StepNumber::FIRST.get(), 1);
        assert_eq!(StepNumber::FIRST.next().unwrap().get(), 2);
        assert_eq!(StepNumber::FIRST.to_string(), "1");
        assert!(StepNumber(NonZeroU32::MAX).next().is_none());
    }
}

//! agent：把一次 turn 推进到答复的完整运行 —— 由多个 step（模型往返）组成。
//!
//! 这是事件循环驱动的 sans-io 状态机，且状态即类型：机器的阶段由
//! [`Ready`]、[`ReadyWithGoal`]、[`AwaitingCall`]、[`AwaitingModel`]、
//! [`AwaitingTools`]、[`Closed`] 表示，[`Agent<Phase>`] 每个阶段只提供推进
//! 该阶段的方法 —— 在等工具时"喂"响应、在等响应时注入、在收场后喂任何
//! 输入，编译期都不可能。开局可以带 goal（[`ReadyWithGoal`]）或不带
//! （[`Ready`]）：前者在 `start` 时把 goal 追加进 turn，后者直接基于
//! 已有历史；`call` 才冻结请求、开启 step 并进入等响应阶段。
//!
//! 机器只持有领域状态、从不记录、也从不执行 IO：[`Agent`] 持有一次 run 的
//! [`Turn`]（消息、模型侧配置、用量、step 记账），转移只交还下一个阶段
//! （或其枚举），阶段本身就是动作提示 —— `AwaitingCall` 表示该调模型，
//! `AwaitingModel` 表示等响应，`AwaitingTools` 表示该跑这批调用。事件与
//! 日志完全属于下游：需要什么由驱动通过访问器读取并自行记录。
//!
//! 只有推进阶段的信息才进机器：完整响应、工具调用的结果、注入与打断。
//! 流式增量不走这里 —— 适配器把产生中的消息直报宿主界面。注入只发生在
//! 调用前的 [`AwaitingCall`]，立即并入会话；缓冲由宿主维护。turn 在最终
//! 答复、打断或 step 预算耗尽时收场，[`Agent::into_result`] 交出整个 run
//! 的结果。

use crate::content::Content;
use crate::header::Environment;
use crate::llm::{LlmResponse, Prompt, StopReason};
use crate::message::{AssistantMessage, Message, ToolCall, ToolCallId, ToolOutput};
use crate::tool::{ToolName, ToolSpec};
use crate::turn::{Outcome, StepNumber, Turn, TurnResult};
use agnes_util::NonEmpty;

/// 阶段：已就绪、尚未开始 —— 没有开局目标。
#[derive(Debug)]
pub struct Ready;

/// 阶段：已就绪、尚未开始 —— goal 还在阶段里，没有进入 [`Turn`]。
#[derive(Debug)]
pub struct ReadyWithGoal {
    goal: Content,
}

/// 阶段：待调用模型 —— 唯一可以注入消息的阶段；`call` 之后进入
/// [`AwaitingModel`]。
#[derive(Debug)]
pub struct AwaitingCall {
    next: StepNumber,
}

/// 阶段：等模型响应 —— 调用已发出，只接受完整响应和打断。
#[derive(Debug)]
pub struct AwaitingModel {
    step: StepNumber,
}

/// 一个调用槽位：只记录调用与是否已落位；结果本身由 [`Turn`] 持有。
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

/// 阶段：已收场 —— 用 [`Agent::into_result`] 取走整个 run 的结果；不再
/// 接受任何输入。
#[derive(Debug)]
pub struct Closed {
    outcome: Outcome,
}

/// 分阶段 typestate 的 Agent：`Agent<Ready>` / `Agent<ReadyWithGoal>` →
/// `Agent<AwaitingCall>` → `Agent<AwaitingModel>` → `Agent<AwaitingTools>` →
///（回到 `Agent<AwaitingCall>` 或收为 `Agent<Closed>`），每一步转移都消耗
/// 当前阶段、交还下一个。
#[derive(Debug)]
pub struct Agent<Phase> {
    turn: Turn,
    phase: Phase,
}

/// 以给定结局收场 —— 所有 `Agent<Closed>` 构造的唯一入口。
fn close(turn: Turn, outcome: Outcome) -> Agent<Closed> {
    Agent {
        turn,
        phase: Closed { outcome },
    }
}

/// 两个就绪阶段共用的开局：进入待调用阶段，step 尚未计步。
fn open_first_call(turn: Turn) -> Agent<AwaitingCall> {
    // 预算非零、step 计数为 0：第一步必然放行，fallback 只是把
    // 这个不变量写成不 panic 的形式。
    let next = turn.next_step_if_allowed().unwrap_or(StepNumber::FIRST);
    Agent {
        turn,
        phase: AwaitingCall { next },
    }
}

impl<Phase> Agent<Phase> {
    /// 本次 run 的领域状态。
    pub fn turn(&self) -> &Turn {
        &self.turn
    }
}

mod sealed {
    pub trait Sealed {}
}

impl sealed::Sealed for Ready {}
impl sealed::Sealed for ReadyWithGoal {}
impl sealed::Sealed for AwaitingCall {}
impl sealed::Sealed for AwaitingModel {}
impl sealed::Sealed for AwaitingTools {}

/// 尚未收场的阶段 —— 只有这些阶段可以增删工具、替换环境。
pub trait OpenPhase: sealed::Sealed {}

impl OpenPhase for Ready {}
impl OpenPhase for ReadyWithGoal {}
impl OpenPhase for AwaitingCall {}
impl OpenPhase for AwaitingModel {}
impl OpenPhase for AwaitingTools {}

impl<P: OpenPhase> Agent<P> {
    /// 未收场阶段的领域状态（可变）；收场后只能通过 [`Agent::into_result`]
    /// 取走结果。
    fn turn_mut(&mut self) -> &mut Turn {
        &mut self.turn
    }

    /// 运行期增加一个工具；重名是 no-op，返回环境是否变化。
    pub fn add_tool(&mut self, spec: ToolSpec) -> bool {
        self.turn_mut().add_tool(spec)
    }

    /// 运行期移除一个工具；不存在是 no-op。
    pub fn remove_tool(&mut self, name: &ToolName) -> bool {
        self.turn_mut().remove_tool(name)
    }

    /// 整体替换工具环境。
    pub fn set_environment(&mut self, environment: Environment) {
        self.turn_mut().set_environment(environment);
    }
}

impl Agent<Ready> {
    /// 构造一个无 goal 的 agent：直接基于已有历史或调用前注入启动。
    pub fn new(turn: Turn) -> Self {
        Self { turn, phase: Ready }
    }

    /// 开始 turn：不追加消息，进入待调用阶段。step 尚未开始计步。
    pub fn start(self) -> Agent<AwaitingCall> {
        open_first_call(self.turn)
    }
}

impl Agent<ReadyWithGoal> {
    /// 构造一个携带 goal 的 agent：goal 留在阶段里，不进入 turn。
    pub fn with_goal(turn: Turn, goal: Content) -> Self {
        Self {
            turn,
            phase: ReadyWithGoal { goal },
        }
    }

    /// 开局目标（尚未进入 turn）。
    pub fn goal(&self) -> &Content {
        &self.phase.goal
    }

    /// 开始 turn：把 goal 追加到 turn，进入待调用阶段。step 尚未开始计步。
    pub fn start(self) -> Agent<AwaitingCall> {
        let Agent { mut turn, phase } = self;
        turn.push_message(Message::User {
            content: phase.goal,
        });
        open_first_call(turn)
    }
}

impl Agent<AwaitingCall> {
    /// 下一次模型调用的 step 编号（尚未开始计步）。
    pub fn next_step(&self) -> StepNumber {
        self.phase.next
    }

    /// 调用前注入一条消息：立即并入会话。缓冲由宿主维护。
    pub fn inject(&mut self, message: Message) {
        self.turn.push_message(message);
    }

    /// 标准的模型输入投影（system + 已生效消息 + 当前工具表）。
    pub fn prompt(&self) -> Prompt {
        self.turn.prompt()
    }

    /// 调用模型：开启 step，进入等响应阶段。
    pub fn call(self) -> Agent<AwaitingModel> {
        let Agent { mut turn, phase } = self;
        let step = turn.begin_step();
        debug_assert_eq!(step, phase.next, "the pre-call step number drifted");
        Agent {
            turn,
            phase: AwaitingModel { step },
        }
    }

    /// 调用前被打断：收场。
    pub fn interrupted(self) -> Agent<Closed> {
        close(self.turn, Outcome::Interrupted)
    }
}

impl Agent<AwaitingModel> {
    pub fn step(&self) -> StepNumber {
        self.phase.step
    }

    /// 一次模型往返的响应到达：用量落账，随后要么转入工具执行，要么收场。
    pub fn respond(self, response: LlmResponse) -> Replied {
        let Agent { mut turn, phase } = self;
        let LlmResponse { stop_reason, usage } = response;
        turn.note_usage(usage);
        match stop_reason {
            StopReason::ToolUse { content, calls } => {
                let slots = calls.map(|call| ToolSlot {
                    call: call.clone(),
                    done: false,
                });
                turn.push_message(Message::Assistant(AssistantMessage::ToolCalls {
                    content,
                    calls,
                }));
                Replied::Tools(Agent {
                    turn,
                    phase: AwaitingTools {
                        step: phase.step,
                        slots,
                    },
                })
            }
            StopReason::Completed { content } => {
                turn.push_message(Message::Assistant(AssistantMessage::Text(content)));
                Replied::Closed(close(turn, Outcome::Answered))
            }
            StopReason::LengthExceeded { content } => {
                if let Some(content) = content {
                    turn.push_message(Message::Assistant(AssistantMessage::Text(content)));
                }
                Replied::Closed(close(turn, Outcome::LengthExceeded))
            }
            StopReason::ContentFiltered { content } => {
                if let Some(content) = content {
                    turn.push_message(Message::Assistant(AssistantMessage::Text(content)));
                }
                Replied::Closed(close(turn, Outcome::ContentFiltered))
            }
        }
    }

    /// 用户打断（响应未达）：turn 以 `Interrupted` 收场，turn 里没有
    /// 被丢弃响应的任何痕迹。
    pub fn interrupted(self) -> Agent<Closed> {
        close(self.turn, Outcome::Interrupted)
    }
}

impl Agent<AwaitingTools> {
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

    /// 一个结果落位：可能仍在等其他调用，可能结算并进入下一次调用前的
    /// 待调用阶段，可能预算耗尽收场。
    ///
    /// 只接受未完成槽位且 id 匹配的结果，顺序不限。未知 id 与重复结果
    /// 是驱动契约违例，不污染 turn。
    ///
    /// # Panics
    ///
    /// debug 构建中，未知或已完成的 `call_id` 会触发 `debug_assert!`；
    /// release 构建幂等忽略。
    pub fn finish(self, call_id: ToolCallId, output: ToolOutput) -> Advanced {
        let Agent {
            mut turn,
            mut phase,
        } = self;
        let Some(slot) = phase.slot_mut(&call_id) else {
            debug_assert!(
                false,
                "tool result for an unknown or already-finished call: {}",
                call_id.as_str()
            );
            return Advanced::Ongoing(Agent { turn, phase });
        };
        slot.done = true;
        turn.push_message(Message::ToolResult { call_id, output });
        if !phase.all_finished() {
            return Advanced::Ongoing(Agent { turn, phase });
        }

        match turn.next_step_if_allowed() {
            Some(next) => Advanced::Next(Agent {
                turn,
                phase: AwaitingCall { next },
            }),
            None => Advanced::Closed(close(turn, Outcome::BudgetExhausted)),
        }
    }

    /// 用户打断：未完成调用的结果以合成错误补齐（按调用顺序），turn 以
    /// `Interrupted` 收场。
    pub fn interrupted(self) -> Agent<Closed> {
        let Agent { mut turn, phase } = self;
        for slot in phase.slots.iter() {
            if !slot.done {
                turn.push_message(Message::ToolResult {
                    call_id: slot.call.id.clone(),
                    output: ToolOutput::not_run(),
                });
            }
        }
        close(turn, Outcome::Interrupted)
    }
}

impl Agent<Closed> {
    /// turn 已收场，借看结果。
    pub fn outcome(&self) -> Outcome {
        self.phase.outcome
    }

    /// 取走收场结果的所有权：结果、用量与走过的 step，一次交清。
    pub fn into_result(self) -> TurnResult {
        let Agent { turn, phase } = self;
        TurnResult {
            outcome: phase.outcome,
            usage: turn.usage(),
            steps_used: turn.steps_started(),
        }
    }
}

/// `respond` 的落点。
#[derive(Debug)]
#[must_use]
pub enum Replied {
    /// 模型要工具：进入执行阶段（全部调用已交给宿主）。
    Tools(Agent<AwaitingTools>),
    /// 答复、输出触顶，或内容被过滤：收场。
    Closed(Agent<Closed>),
}

/// `finish` 的落点。
#[derive(Debug)]
#[must_use]
pub enum Advanced {
    /// 结果落位但未收齐：继续等其余调用。
    Ongoing(Agent<AwaitingTools>),
    /// 全部落位并结算：下一次模型调用已就绪（尚未 `call`）。
    Next(Agent<AwaitingCall>),
    /// 预算耗尽：turn 收场。
    Closed(Agent<Closed>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{TokenUsage, ToolArguments, ToolOutput};
    use crate::test_support::{budget, content, header, user};
    use crate::tool::{JsonSchema, ToolName};
    use std::str::FromStr;

    fn new_turn(steps: u32) -> Turn {
        Turn::new(header(), budget(steps))
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

    fn tool_results(turn: &Turn) -> Vec<String> {
        turn.messages()
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult { call_id, .. } => Some(call_id.as_str().to_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_new_turn_keeps_the_goal_out_of_the_turn_until_start() {
        let agent = Agent::with_goal(new_turn(4), content("flip the flag"));
        assert!(agent.turn().messages().is_empty());
        assert_eq!(agent.turn().steps_started(), 0);
        assert_eq!(agent.goal().as_str(), "flip the flag");

        let agent = agent.start();
        assert_eq!(agent.turn().messages(), &[user("flip the flag")]);
    }

    #[test]
    fn a_goal_less_turn_starts_without_adding_a_message() {
        let agent = Agent::new(new_turn(4));
        assert!(agent.turn().messages().is_empty());

        let agent = agent.start();
        assert!(agent.turn().messages().is_empty());
        assert_eq!(agent.next_step().get(), 1);
        assert_eq!(agent.prompt().input.len(), 1); // system only
        assert_eq!(agent.call().step().get(), 1);
    }

    #[test]
    fn a_goal_less_turn_can_be_driven_by_an_injection() {
        let mut agent = Agent::new(new_turn(4)).start();
        agent.inject(user("summarize the repo"));

        let prompt = agent.prompt();
        assert_eq!(prompt.input.len(), 2);
        assert!(matches!(
            &prompt.input[1],
            Message::User { content } if content.as_str() == "summarize the repo"
        ));
    }

    #[test]
    fn a_started_turn_waits_for_the_call_without_burning_a_step() {
        let agent = Agent::with_goal(new_turn(4), content("flip the flag")).start();
        assert_eq!(agent.next_step().get(), 1);
        assert_eq!(agent.turn().steps_started(), 0);
        assert_eq!(agent.prompt().input.len(), 2);

        let agent = agent.call();
        assert_eq!(agent.step().get(), 1);
        assert_eq!(agent.turn().steps_started(), 1);
    }

    #[test]
    fn injections_are_immediate_before_the_call() {
        let mut agent = Agent::with_goal(new_turn(4), content("flip")).start();
        agent.inject(user("early"));

        assert_eq!(agent.turn().messages(), &[user("flip"), user("early")]);
        assert_eq!(agent.prompt().input.len(), 3);
        assert_eq!(agent.call().step().get(), 1);
    }

    #[test]
    fn interrupting_before_the_call_keeps_the_step_count() {
        let mut agent = Agent::with_goal(new_turn(4), content("flip")).start();
        agent.inject(user("hold on"));
        let agent = agent.interrupted();

        assert_eq!(agent.outcome(), Outcome::Interrupted);
        assert_eq!(agent.turn().messages(), &[user("flip"), user("hold on")]);
        assert_eq!(agent.into_result().steps_used, 0);
    }

    #[test]
    fn start_appends_the_goal_before_the_first_call() {
        let agent = Agent::with_goal(new_turn(4), content("flip the flag")).start();
        assert_eq!(agent.next_step().get(), 1);
        assert_eq!(agent.turn().messages(), &[user("flip the flag")]);
        assert_eq!(agent.turn().steps_started(), 0);

        let prompt = agent.prompt();
        assert_eq!(prompt.input.len(), 2);
        assert!(matches!(prompt.input[0], Message::System { .. }));
        assert!(
            matches!(&prompt.input[1], Message::User { content } if content.as_str() == "flip the flag")
        );
    }

    #[test]
    fn a_tool_use_response_hands_out_every_call_at_once() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) = agent.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };

        let calls: Vec<&str> = agent.calls().map(|call| call.id.as_str()).collect();
        assert_eq!(calls, ["c1", "c2"]);
        assert_eq!(
            agent.next_unfinished().map(|call| call.id.as_str()),
            Some("c1")
        );
        assert!(matches!(
            agent.turn().messages().last(),
            Some(Message::Assistant(AssistantMessage::ToolCalls { .. }))
        ));
    }

    #[test]
    fn results_may_arrive_in_any_order() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) = agent.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };

        let Advanced::Ongoing(agent) = agent.finish(ToolCallId::new("c2"), output()) else {
            panic!("expected Ongoing");
        };
        assert_eq!(
            agent.next_unfinished().map(|call| call.id.as_str()),
            Some("c1")
        );

        let Advanced::Next(agent) = agent.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Next");
        };
        assert_eq!(agent.next_step().get(), 2);
        assert_eq!(tool_results(agent.turn()), ["c2", "c1"]);

        // system, goal, assistant(tool calls), result c2, result c1
        assert_eq!(agent.prompt().input.len(), 5);
        assert_eq!(agent.call().step().get(), 2);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn unknown_and_duplicate_results_are_ignored_in_release() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) = agent.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };

        let Advanced::Ongoing(agent) = agent.finish(ToolCallId::new("nope"), output()) else {
            panic!("expected Ongoing");
        };
        let Advanced::Ongoing(agent) = agent.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Ongoing");
        };
        // 重复结果不产生第二条结果消息
        let Advanced::Ongoing(agent) = agent.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Ongoing");
        };
        assert_eq!(tool_results(agent.turn()), ["c1"]);

        let Advanced::Next(_) = agent.finish(ToolCallId::new("c2"), output()) else {
            panic!("expected Next");
        };
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "unknown or already-finished call")]
    fn an_unknown_result_is_a_driver_contract_violation() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) = agent.respond(response(tool_use(vec![call("c1")]))) else {
            panic!("expected Tools");
        };
        let _ = agent.finish(ToolCallId::new("nope"), output());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "unknown or already-finished call")]
    fn a_duplicate_result_is_a_driver_contract_violation() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) = agent.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };
        let Advanced::Ongoing(agent) = agent.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Ongoing");
        };
        let _ = agent.finish(ToolCallId::new("c1"), output());
    }

    #[test]
    fn duplicate_call_ids_still_complete_without_livelock() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) =
            agent.respond(response(tool_use(vec![call("dup"), call("dup")])))
        else {
            panic!("expected Tools");
        };
        let Advanced::Ongoing(agent) = agent.finish(ToolCallId::new("dup"), output()) else {
            panic!("expected Ongoing");
        };
        let Advanced::Next(agent) = agent.finish(ToolCallId::new("dup"), output()) else {
            panic!("expected Next");
        };
        assert_eq!(tool_results(agent.turn()), ["dup", "dup"]);
    }

    #[test]
    fn an_answer_ends_the_turn() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Closed(agent) = agent.respond(response(completed("done it"))) else {
            panic!("expected Closed");
        };
        assert_eq!(agent.outcome(), Outcome::Answered);
        assert!(matches!(
            agent.turn().messages(),
            [Message::User { .. }, Message::Assistant(AssistantMessage::Text(text))]
                if text.as_str() == "done it"
        ));
        let result = agent.into_result();
        assert_eq!(result.steps_used, 1);
    }

    #[test]
    fn the_budget_ends_the_turn_after_a_tool_round() {
        let agent = Agent::with_goal(new_turn(1), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) = agent.respond(response(tool_use(vec![call("c1")]))) else {
            panic!("expected Tools");
        };
        let Advanced::Closed(agent) = agent.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Closed");
        };
        assert_eq!(agent.outcome(), Outcome::BudgetExhausted);
    }

    #[test]
    fn injections_take_effect_before_the_next_call() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) = agent.respond(response(tool_use(vec![call("c1")]))) else {
            panic!("expected Tools");
        };
        let Advanced::Next(mut agent) = agent.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Next");
        };
        agent.inject(user("also check the tests"));

        let prompt = agent.prompt();
        assert!(matches!(
            prompt.input.last(),
            Some(Message::User { content }) if content.as_str() == "also check the tests"
        ));
        assert_eq!(agent.call().step().get(), 2);
    }

    #[test]
    fn length_and_filtered_responses_close_the_turn() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Closed(agent) =
            agent.respond(response(StopReason::LengthExceeded { content: None }))
        else {
            panic!("expected Closed");
        };
        assert_eq!(agent.outcome(), Outcome::LengthExceeded);

        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Closed(agent) = agent.respond(response(StopReason::ContentFiltered {
            content: Some(content("partial")),
        })) else {
            panic!("expected Closed");
        };
        assert_eq!(agent.outcome(), Outcome::ContentFiltered);
        assert!(matches!(
            agent.turn().messages().last(),
            Some(Message::Assistant(AssistantMessage::Text(text))) if text.as_str() == "partial"
        ));
    }

    #[test]
    fn interrupting_the_model_wait_closes_after_the_call() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let agent = agent.interrupted();
        assert_eq!(agent.outcome(), Outcome::Interrupted);
        let result = agent.into_result();
        assert_eq!(result.steps_used, 1);
    }

    #[test]
    fn interrupting_the_tools_fills_synthetic_results() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) = agent.respond(response(tool_use(vec![call("c1"), call("c2")])))
        else {
            panic!("expected Tools");
        };
        let Advanced::Ongoing(agent) = agent.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Ongoing");
        };

        let agent = agent.interrupted();
        assert_eq!(agent.outcome(), Outcome::Interrupted);
        let messages = agent.turn().messages();
        assert!(matches!(
            messages.last(),
            Some(Message::ToolResult { call_id, output })
                if call_id.as_str() == "c2" && output.is_error()
        ));
        let result = agent.into_result();
        assert_eq!(result.steps_used, 1);
    }

    #[test]
    fn usage_and_steps_accumulate_across_steps() {
        let agent = Agent::with_goal(new_turn(4), content("flip"))
            .start()
            .call();
        let Replied::Tools(agent) = agent.respond(response(tool_use(vec![call("c1")]))) else {
            panic!("expected Tools");
        };
        let Advanced::Next(agent) = agent.finish(ToolCallId::new("c1"), output()) else {
            panic!("expected Next");
        };
        let Replied::Closed(agent) = agent.call().respond(response(completed("done"))) else {
            panic!("expected Closed");
        };

        let result = agent.into_result();
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
        let mut agent = Agent::with_goal(new_turn(4), content("flip")).start();
        let spec = ToolSpec::new(
            ToolName::from_str("injected").unwrap(),
            "an injected tool",
            JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        );
        assert!(agent.add_tool(spec));
        assert_eq!(agent.prompt().tools.len(), 1);
        assert!(agent.remove_tool(&ToolName::from_str("injected").unwrap()));
        assert!(agent.prompt().tools.is_empty());
    }
}

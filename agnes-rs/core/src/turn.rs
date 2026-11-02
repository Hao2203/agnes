//! Turn：一个 turn 携带的全部领域状态 —— 会话消息、模型侧配置、
//! 用量与 step 记账，以及开局配置与收场词汇。
//!
//! 状态机不记录日志、不产出事件：需要什么由下游通过这里的访问器读取。
//! 系统提示只有 [`Turn::header`] 一个表示，[`Turn::prompt`] 把它
//! 投影成模型输入的第一条 `system` 消息。

use crate::config::StepBudget;
use crate::content::Content;
use crate::header::{Environment, Header};
use crate::llm::Prompt;
use crate::message::{Message, TokenUsage};
use crate::tool::{ToolName, ToolSpec};
use std::fmt;
use std::num::NonZeroU32;

/// 一个 turn 的领域状态。
#[derive(Debug)]
pub struct Turn {
    header: Header,
    messages: Vec<Message>,
    budget: StepBudget,
    usage: TokenUsage,
    steps_started: u32,
}

impl Turn {
    /// 全新的 turn：没有历史消息，step 计数从未开始。
    pub fn new(header: Header, budget: StepBudget) -> Self {
        Self {
            header,
            messages: Vec::new(),
            budget,
            usage: TokenUsage::default(),
            steps_started: 0,
        }
    }

    /// 带既有会话历史重建 turn（跨 run 续跑）。消息按发生顺序保留；
    /// 系统提示与工具环境以当前 header 为准。
    pub fn with_messages(header: Header, budget: StepBudget, messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Self::new(header, budget)
        }
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// 会话消息，按发生顺序；header 的系统提示不在这里，由
    /// [`Turn::prompt`] 投影为第一条。`System` 消息与其他角色一样，
    /// 只是普通会话消息的一种。
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn usage(&self) -> TokenUsage {
        self.usage
    }

    pub fn steps_started(&self) -> u32 {
        self.steps_started
    }

    /// 标准的模型输入投影：system prompt、全部消息、当前工具表。
    pub fn prompt(&self) -> Prompt {
        let mut input = Vec::with_capacity(self.messages.len() + 1);
        input.push(Message::System {
            content: self.header.system().clone(),
        });
        input.extend(self.messages.iter().cloned());
        Prompt {
            input,
            tools: self.header.environment().specs(),
        }
    }

    /// 运行期增加一个工具；重名是 no-op。
    pub fn add_tool(&mut self, spec: ToolSpec) -> bool {
        self.header.environment_mut().add(spec)
    }

    /// 运行期移除一个工具；不存在是 no-op。
    pub fn remove_tool(&mut self, name: &ToolName) -> bool {
        self.header.environment_mut().remove(name)
    }

    /// 整体替换工具环境。
    pub fn set_environment(&mut self, environment: Environment) {
        *self.header.environment_mut() = environment;
    }

    pub(crate) fn push_message(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// 预算允许时返回下一个 step 的编号（尚未计步）；预算耗尽返回 `None`。
    /// 预算判定只在这里发生。
    pub(crate) fn next_step_if_allowed(&self) -> Option<StepNumber> {
        self.budget
            .allows(self.steps_started)
            .then(|| self.next_step())
    }

    /// 开启一个已由 [`Turn::next_step_if_allowed`] 放行的 step。
    pub(crate) fn begin_step(&mut self) -> StepNumber {
        debug_assert!(
            self.budget.allows(self.steps_started),
            "a step can only begin when the budget allows it"
        );
        let step = self.next_step();
        self.steps_started = step.get();
        step
    }

    /// 下一个 step 的编号（不判定预算）。调用方必须已经放行。
    fn next_step(&self) -> StepNumber {
        // steps_started < 预算 ≤ u32::MAX，这里加一不会溢出。
        let number = self.steps_started + 1;
        StepNumber::new(NonZeroU32::new(number).expect("step count fits a u32"))
    }

    pub(crate) fn note_usage(&mut self, usage: TokenUsage) {
        self.usage = self.usage.saturating_add(usage);
    }
}

/// 把一个 turn 带起来的全部输入：这一轮生效的模型侧配置、可选的
/// 开局目标消息，以及最多可以启动多少个模型往返。`goal` 为 `None` 时
/// 直接基于已有会话历史或调用前注入启动。已有会话历史不在这里 —— 它由
/// 宿主重建进 [`Turn`]。
#[derive(Debug)]
pub struct AgentConfig {
    pub header: Header,
    pub goal: Option<Content>,
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

/// turn 收场的结果：怎么收场、用量与走过的 step。
#[derive(Debug)]
#[must_use]
pub struct TurnResult {
    pub outcome: Outcome,
    pub usage: TokenUsage,
    pub steps_used: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemPrompt;
    use crate::content::Content;
    use crate::tool::JsonSchema;
    use std::str::FromStr;

    fn header() -> Header {
        Header::new(SystemPrompt::new("be brief"), Environment::default())
    }

    fn budget(steps: u32) -> StepBudget {
        StepBudget::new(NonZeroU32::new(steps).unwrap())
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(
            ToolName::from_str(name).unwrap(),
            "",
            JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        )
    }

    fn user(text: &str) -> Message {
        Message::User {
            content: Content::try_from(text).unwrap(),
        }
    }

    #[test]
    fn the_prompt_starts_with_the_system_prompt_and_keeps_message_order() {
        let mut turn =
            Turn::with_messages(header(), budget(4), vec![user("earlier"), user("goal")]);
        turn.add_tool(spec("read_file"));

        let prompt = turn.prompt();
        assert_eq!(prompt.input.len(), 3);
        assert!(
            matches!(&prompt.input[0], Message::System { content } if content.as_str() == "be brief")
        );
        assert!(
            matches!(&prompt.input[1], Message::User { content } if content.as_str() == "earlier")
        );
        assert!(
            matches!(&prompt.input[2], Message::User { content } if content.as_str() == "goal")
        );
        assert_eq!(prompt.tools.len(), 1);
        assert_eq!(prompt.tools[0].name().as_str(), "read_file");
    }

    #[test]
    fn tools_are_added_and_removed_by_spec() {
        let mut turn = Turn::new(header(), budget(4));
        let name = ToolName::from_str("read_file").unwrap();
        assert!(turn.add_tool(spec("read_file")));
        assert!(!turn.add_tool(spec("read_file")));
        assert!(turn.header().environment().contains(&name));
        assert!(turn.remove_tool(&name));
        assert!(!turn.header().environment().contains(&name));
    }

    #[test]
    fn the_budget_gate_stops_after_the_last_step() {
        let mut turn = Turn::new(header(), budget(1));
        assert_eq!(turn.next_step_if_allowed().map(StepNumber::get), Some(1));
        assert_eq!(turn.begin_step().get(), 1);
        assert!(turn.next_step_if_allowed().is_none());
    }

    #[test]
    fn steps_and_usage_accumulate() {
        let mut turn = Turn::new(header(), budget(4));
        assert_eq!(turn.next_step_if_allowed().map(StepNumber::get), Some(1));
        assert_eq!(turn.begin_step().get(), 1);
        assert_eq!(turn.next_step_if_allowed().map(StepNumber::get), Some(2));
        assert_eq!(turn.begin_step().get(), 2);
        turn.note_usage(TokenUsage {
            input: 2,
            output: 3,
        });
        turn.note_usage(TokenUsage {
            input: 1,
            output: 1,
        });
        assert_eq!(
            turn.usage(),
            TokenUsage {
                input: 3,
                output: 4
            }
        );
        assert_eq!(turn.steps_started(), 2);
    }

    #[test]
    fn step_numbers_count_from_one_and_stop_at_the_limit() {
        assert_eq!(StepNumber::FIRST.get(), 1);
        assert_eq!(StepNumber::FIRST.next().unwrap().get(), 2);
        assert_eq!(StepNumber::FIRST.to_string(), "1");
        assert!(StepNumber(NonZeroU32::MAX).next().is_none());
    }
}

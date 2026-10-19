//! 宿主驱动状态机：管理事件、指令、输入、输出与状态。
//!
//! 机器的输出（动作）经 [`Driver::pump_actions`] 铸造为事件进入
//! **事件队列**；[`Driver::drain_events`] 逐条出队 —— core 镜像先交给插件
//! 再上报宿主，工具调用请求先记下待办、交由插件链发指令（无人表态则系统
//! 默认放行），问询答覆只作观测。插件发下的指令进 **指令队列**，
//! [`Driver::execute_command`] 逐条执行：放行向机器喂 `tool_started` 并
//! 转入等工具；拒绝向机器喂合成失败结果；`Ask` 转入等审批（未接线即
//! fail-closed 拒绝）。执行的输入又可能让机器产出新动作 —— 反应循环
//! （[`Driver::react`]）一直转到机器没有新动作、两条队列都空。
//!
//! **状态**只有"在等什么"：等模型响应 / 等工具执行 / 等审批答覆，三种
//! 等待各自 select! 打断，结果作为输入喂回机器后回到反应循环，直到机器
//! 收场。审批的落定也统一经指令队列回流 —— 系统对自己发的指令与插件的
//! 指令走同一条执行路径。

use std::collections::VecDeque;
use std::mem;
use std::time::Duration;

use agnes_core::content::Content;
use agnes_core::context::Context;
use agnes_core::llm::{LlmRequest, LlmResponse, RequestBuilder};
use agnes_core::message::{ToolCall, ToolCallId, ToolOutput};
use agnes_core::turn::{
    Action, AwaitingMessage, AwaitingTools, Closed, MessageReplied, ToolsAdvanced, Turn,
};
use agnes_io::input::Input as InputPort;
use agnes_io::llm::Llm;
use agnes_io::tool::ToolRegistry;
use jiff::Timestamp;
use tokio::sync::oneshot::Receiver as AnswerReceiver;

use crate::approval::{ApprovalChannel, ApprovalRequest};
use crate::plugin::{Command, PluginChain, PluginEvent};
use crate::{AgentError, Event, Sink};

/// 一次驱动的固定接线：全部被动 IO 端与策略注入点。
pub(crate) struct Ports<'a> {
    pub(crate) registry: &'a ToolRegistry,
    pub(crate) llm: &'a dyn Llm,
    pub(crate) input: &'a dyn InputPort,
    pub(crate) requests: &'a dyn RequestBuilder,
    pub(crate) sink: &'a mut dyn Sink,
    pub(crate) plugins: &'a mut PluginChain,
    pub(crate) approvals: Option<&'a ApprovalChannel>,
}

/// core turn 的阶段化封装 —— 驱动状态机的动态货币。
/// （core 保持自己的 typestate；驱动需要在运行时以动态状态持有它。）
enum Machine {
    AwaitingMessage(Turn<AwaitingMessage>),
    AwaitingTools(Turn<AwaitingTools>),
    Closed(Turn<Closed>),
    /// 转移期间的中转占位，只在 [`Driver::transition`] 的换绑瞬间存在，
    /// 不可观察。
    Transient,
}

impl Machine {
    fn poll_actions(&mut self) -> Option<Action> {
        match self {
            Machine::AwaitingMessage(turn) => turn.poll_actions(),
            Machine::AwaitingTools(turn) => turn.poll_actions(),
            Machine::Closed(turn) => turn.poll_actions(),
            Machine::Transient => unreachable!("中转状态不可观察"),
        }
    }

    fn context(&self) -> &Context {
        match self {
            Machine::AwaitingMessage(turn) => turn.context(),
            Machine::AwaitingTools(turn) => turn.context(),
            Machine::Closed(turn) => turn.context(),
            Machine::Transient => unreachable!("中转状态不可观察"),
        }
    }

    fn user_message(&mut self, content: Content) {
        match self {
            Machine::AwaitingMessage(turn) => turn.user_message(content),
            Machine::AwaitingTools(turn) => turn.user_message(content),
            Machine::Closed(_) => {} // 收场后的插话不进机器
            Machine::Transient => unreachable!("中转状态不可观察"),
        }
    }

    fn tool_started(&mut self, call_id: &ToolCallId) {
        match self {
            Machine::AwaitingTools(turn) => turn.tool_started(call_id),
            _ => debug_assert!(false, "只有等工具的机器能开始执行调用"),
        }
    }

    fn model_responded(self, response: LlmResponse) -> Self {
        match self {
            Machine::AwaitingMessage(turn) => match turn.model_responded(response) {
                MessageReplied::Tools { turn } => Machine::AwaitingTools(turn),
                MessageReplied::Closed { turn } => Machine::Closed(turn),
            },
            other => {
                debug_assert!(false, "等工具/已收场的机器不接模型响应");
                other
            }
        }
    }

    fn tool_finished(self, call_id: ToolCallId, output: ToolOutput) -> Self {
        match self {
            Machine::AwaitingTools(turn) => match turn.tool_finished(call_id, output) {
                ToolsAdvanced::Ongoing { turn } => Machine::AwaitingTools(turn),
                ToolsAdvanced::NextStep { turn } => Machine::AwaitingMessage(turn),
                ToolsAdvanced::Closed { turn } => Machine::Closed(turn),
            },
            other => {
                debug_assert!(false, "等消息/已收场的机器不接工具结果");
                other
            }
        }
    }

    fn interrupted(self) -> Self {
        match self {
            Machine::AwaitingMessage(turn) => Machine::Closed(turn.interrupted()),
            Machine::AwaitingTools(turn) => Machine::Closed(turn.interrupted()),
            closed @ Machine::Closed(_) => closed,
            Machine::Transient => Machine::Transient,
        }
    }
}

/// 驱动的状态：在等什么。
enum Waiting {
    /// 解释态：机器动作或队列还有活（反应循环的驻留态）。
    Reacting,
    /// 等模型响应。
    AwaitingModel { request: LlmRequest },
    /// 等工具执行结果。
    AwaitingTool { call: ToolCall },
    /// 等审批答覆（可选超时）。
    AwaitingApproval {
        call: ToolCall,
        answer: AnswerReceiver<bool>,
        timeout: Option<Duration>,
    },
}

/// 挂起的审批问询如何落定。
enum Resolution {
    /// 批准 —— 照常执行。
    Approved,
    /// 拒绝，附理由。
    Denied(String),
    /// 等待被打断 —— 整个 run 收场。
    Interrupted,
}

/// 驱动状态机：事件队列 → 插件链 → 指令队列 → 系统执行。
pub(crate) struct Driver<'a> {
    ports: Ports<'a>,
    machine: Machine,
    state: Waiting,
    /// 事件队列：core 镜像与系统事件，出口是插件链（镜像同步上报宿主）。
    events: VecDeque<PluginEvent>,
    /// 指令队列：插件（及系统自身）发下，由系统按序执行。
    commands: VecDeque<Command>,
    /// 已出队、等待指令定案的调用请求。
    requested: Option<ToolCall>,
}

impl<'a> Driver<'a> {
    pub(crate) fn new(turn: Turn<AwaitingMessage>, ports: Ports<'a>) -> Self {
        Self {
            ports,
            machine: Machine::AwaitingMessage(turn),
            state: Waiting::Reacting,
            events: VecDeque::new(),
            commands: VecDeque::new(),
            requested: None,
        }
    }

    /// 阶段转移的换绑：机器转移消耗自身，经中转占位换回新状态。
    fn transition(&mut self, transfer: impl FnOnce(Machine) -> Machine) {
        let taken = mem::replace(&mut self.machine, Machine::Transient);
        self.machine = transfer(taken);
    }

    /// 一路驱动到收场。
    pub(crate) async fn run(mut self) -> Result<Turn<Closed>, AgentError> {
        loop {
            // 输入：运行中的插话随时进机器缓存；生效时机由机器按 step
            // 边界决定。
            for content in self.ports.input.drain() {
                self.machine.user_message(content);
            }
            self.react();
            if let Machine::Closed(_) = self.machine {
                break;
            }
            match mem::replace(&mut self.state, Waiting::Reacting) {
                Waiting::AwaitingModel { request } => {
                    let raced = tokio::select! {
                        biased;
                        () = self.ports.input.interrupted() => None,
                        response = self.ports.llm.complete(&request) => Some(response),
                    };
                    match raced {
                        None => self.transition(Machine::interrupted),
                        Some(Ok(response)) => {
                            self.transition(|machine| machine.model_responded(response));
                        }
                        Some(Err(err)) => return Err(AgentError::Llm(err)),
                    }
                }
                Waiting::AwaitingTool { call } => {
                    let raced = tokio::select! {
                        biased;
                        () = self.ports.input.interrupted() => None,
                        output = self.ports.registry.execute(&call) => Some(output),
                    };
                    match raced {
                        None => self.transition(Machine::interrupted),
                        Some(output) => {
                            self.transition(|machine| machine.tool_finished(call.id, output));
                        }
                    }
                }
                Waiting::AwaitingApproval {
                    call,
                    answer,
                    timeout,
                } => {
                    let resolution = match timeout {
                        None => tokio::select! {
                            biased;
                            () = self.ports.input.interrupted() => Resolution::Interrupted,
                            answer = answer => match answer {
                                Ok(true) => Resolution::Approved,
                                Ok(false) => Resolution::Denied("the approval was refused".into()),
                                Err(_) => Resolution::Denied("the approver went away".into()),
                            },
                        },
                        Some(limit) => tokio::select! {
                            biased;
                            () = self.ports.input.interrupted() => Resolution::Interrupted,
                            resolution = tokio::time::timeout(limit, answer) => match resolution {
                                Ok(Ok(true)) => Resolution::Approved,
                                Ok(Ok(false)) => Resolution::Denied("the approval was refused".into()),
                                Ok(Err(_recv)) => Resolution::Denied("the approver went away".into()),
                                Err(_elapsed) => Resolution::Denied("the approval timed out".into()),
                            },
                        },
                    };
                    self.ports.sink.event(&Event::ApprovalResolved {
                        at: Timestamp::now(),
                        call_id: call.id.clone(),
                        approved: matches!(resolution, Resolution::Approved),
                    });
                    // 答覆作为观测事件回流；落定统一经指令队列 —— 系统对
                    // 自己发的指令与插件的指令走同一条执行路径。
                    let approved = matches!(resolution, Resolution::Approved);
                    self.events.push_back(PluginEvent::ApprovalAnswered {
                        call: call.clone(),
                        approved,
                    });
                    match resolution {
                        Resolution::Interrupted => {
                            self.transition(Machine::interrupted);
                        }
                        Resolution::Approved => {
                            self.requested = Some(call);
                            self.commands.push_back(Command::Allow);
                        }
                        Resolution::Denied(reason) => {
                            self.requested = Some(call);
                            self.commands.push_back(Command::Deny(reason));
                        }
                    }
                }
                Waiting::Reacting => unreachable!("反应循环只在摆好等待点或收场后退出"),
            }
        }
        match self.machine {
            Machine::Closed(turn) => Ok(turn),
            _ => unreachable!("循环只在机器收场后退出"),
        }
    }

    /// 反应循环：把机器动作、事件队列、指令队列处理到静止。
    fn react(&mut self) {
        loop {
            self.pump_actions();
            self.drain_events();
            let mut fed = false;
            while let Some(command) = self.commands.pop_front() {
                fed |= self.execute_command(command);
            }
            // 静止：没有新指令、没有待处理事件、也没有刚喂出的新动作。
            if !fed && self.events.is_empty() && self.commands.is_empty() {
                break;
            }
        }
    }

    /// 机器输出 → 事件队列 / 等待目标。
    fn pump_actions(&mut self) {
        while let Some(action) = self.machine.poll_actions() {
            match action {
                Action::Report(event) => self.events.push_back(PluginEvent::Core(event)),
                Action::CallModel => {
                    let request = self.ports.requests.build(self.machine.context());
                    self.state = Waiting::AwaitingModel { request };
                }
                Action::RunTool(call) => self
                    .events
                    .push_back(PluginEvent::ToolCallRequested { call }),
            }
        }
    }

    /// 事件队列出队：core 镜像上报宿主并交给插件；工具调用请求交给插件
    /// 链发指令（无人表态则系统默认放行）；问询答覆只作观测。
    fn drain_events(&mut self) {
        while let Some(event) = self.events.pop_front() {
            match &event {
                PluginEvent::Core(core) => {
                    let command = self.ports.plugins.on_event(&event);
                    debug_assert!(command.is_none(), "指令只对工具调用请求有效");
                    self.ports.sink.event(&Event::Recorded {
                        at: Timestamp::now(),
                        event: core.clone(),
                    });
                }
                PluginEvent::ToolCallRequested { call } => {
                    debug_assert!(self.requested.is_none(), "同一时刻只可能有一个待定调用");
                    self.requested = Some(call.clone());
                    let command = self
                        .ports
                        .plugins
                        .on_event(&event)
                        .unwrap_or(Command::Allow);
                    self.commands.push_back(command);
                }
                PluginEvent::ApprovalAnswered { .. } => {
                    let command = self.ports.plugins.on_event(&event);
                    debug_assert!(command.is_none(), "指令只对工具调用请求有效");
                }
            }
        }
    }

    /// 系统执行一条指令；返回是否向机器喂了输入（还有新输出要泵）。
    fn execute_command(&mut self, command: Command) -> bool {
        let Some(call) = self.requested.take() else {
            debug_assert!(false, "指令没有对应的调用请求");
            return false;
        };
        match command {
            Command::Allow => {
                self.machine.tool_started(&call.id);
                self.state = Waiting::AwaitingTool { call };
                true
            }
            Command::Deny(reason) => {
                self.deny_tool(call, reason);
                true
            }
            Command::Ask => match self.ports.approvals {
                None => {
                    self.deny_tool(
                        call,
                        "tool requires an approval, but no approval channel is wired".into(),
                    );
                    true
                }
                Some(channel) => {
                    let (reply, answer) = tokio::sync::oneshot::channel::<bool>();
                    let request = ApprovalRequest {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        reply,
                    };
                    if let Err(closed) = channel.send(request) {
                        self.deny_tool(call, format!("the approval channel was closed: {closed}"));
                        return true;
                    }
                    self.ports.sink.event(&Event::ApprovalRequested {
                        at: Timestamp::now(),
                        call: call.clone(),
                    });
                    self.state = Waiting::AwaitingApproval {
                        call,
                        answer,
                        timeout: channel.timeout(),
                    };
                    false
                }
            },
        }
    }

    /// 以一次合成拒绝推进机器：gating 事件先上报，随后 tool_start +
    /// 失败结果落位 —— 模型看得见拒绝并自行调整。
    fn deny_tool(&mut self, call: ToolCall, reason: String) {
        self.ports.sink.event(&Event::ToolGated {
            at: Timestamp::now(),
            call: call.clone(),
            reason: reason.clone(),
        });
        self.machine.tool_started(&call.id);
        self.transition(|machine| {
            machine.tool_finished(
                call.id,
                ToolOutput::error(format!("denied by policy: {reason}")),
            )
        });
    }
}

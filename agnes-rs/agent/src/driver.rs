//! 宿主驱动状态机：把 core 的 typestate 与宿主的 IO / 策略接起来。
//!
//! 机器产出的动作在 [`Driver::pump_actions`] 里**就地**处理：报告类动作
//! 先给扩展内核观察、再上报宿主；`CallModel` 组装请求；`RunTool` 走门禁
//! 折叠（放行 / 审批 / 拒绝）。没有事件队列与指令队列 —— 类型化钩子在
//! 调用点直接给出决议，系统就地执行。
//!
//! **状态**只有「在等什么」：等模型响应 / 等工具执行 / 等审批答覆。三个
//! 等待点都用 `select!` 与打断赛跑；审批落定后就地执行或合成拒绝。
//!
//! core 的封闭枚举不因插件扩展：拒绝的合成结果、问询的构造与挂起、结果
//! 的替换都由驱动完成，机器只看到普通的工具往返。

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
use agnes_io::session::SessionLog;
use agnes_io::tool::ToolRegistry;
use jiff::Timestamp;
use tokio::sync::oneshot::Receiver as AnswerReceiver;

use crate::approval::{ApprovalChannel, ApprovalRequest};
use crate::ext::{ApprovalOutcome, Denied, Extensions, Gate, Notice};
use crate::{AgentError, Event, Sink};

/// 一次驱动的固定接线：全部被动 IO 端与策略注入点。
pub(crate) struct Ports<'a> {
    pub(crate) registry: &'a ToolRegistry,
    pub(crate) llm: &'a dyn Llm,
    pub(crate) input: &'a dyn InputPort,
    pub(crate) requests: &'a dyn RequestBuilder,
    pub(crate) extensions: &'a mut Extensions,
    pub(crate) approvals: Option<&'a ApprovalChannel>,
}

/// 一次 run 的出口：上报与落盘只在这次 run 内借用。
pub(crate) struct Outlets<'s, 'q> {
    pub(crate) sink: &'s mut dyn Sink,
    pub(crate) session: Option<&'q mut dyn SessionLog>,
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
    /// 中转态：反应循环换绑时的占位，不可观察。
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
    /// 批准一次 —— 照常执行。
    Allowed,
    /// 拒绝；`outcome` 是 fail-closed 词汇，`detail` 是给模型与界面的理由。
    Denied {
        outcome: ApprovalOutcome,
        detail: &'static str,
    },
    /// 等待被打断 —— 整个 run 收场。
    Interrupted,
}

impl Resolution {
    fn outcome(&self) -> ApprovalOutcome {
        match self {
            Resolution::Allowed => ApprovalOutcome::AllowedOnce,
            Resolution::Denied { outcome, .. } => *outcome,
            Resolution::Interrupted => ApprovalOutcome::Cancelled,
        }
    }
}

/// 驱动状态机。
pub(crate) struct Driver<'a, 's, 'q> {
    ports: Ports<'a>,
    outlets: Outlets<'s, 'q>,
    machine: Machine,
    state: Waiting,
}

impl<'a, 's, 'q> Driver<'a, 's, 'q> {
    pub(crate) fn new(
        turn: Turn<AwaitingMessage>,
        ports: Ports<'a>,
        outlets: Outlets<'s, 'q>,
    ) -> Self {
        Self {
            ports,
            outlets,
            machine: Machine::AwaitingMessage(turn),
            state: Waiting::Reacting,
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
            self.pump_actions().await?;
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
                            // 结果折叠：插件可以在入账前挡回，反馈仍是一条
                            // 错误结果 —— 调用/结果始终一一对应。
                            let output = if let Some(blocked) =
                                self.ports.extensions.settle(&call, &output)
                            {
                                self.ports.extensions.observe(&Notice::Blocked {
                                    call: &call,
                                    by: blocked.by,
                                    feedback: &blocked.feedback,
                                });
                                ToolOutput::error(blocked.feedback)
                            } else {
                                self.ports.extensions.observe(&Notice::Ran {
                                    call: &call,
                                    output: &output,
                                });
                                output
                            };
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
                                Ok(true) => Resolution::Allowed,
                                Ok(false) => Resolution::Denied {
                                    outcome: ApprovalOutcome::Rejected,
                                    detail: "the approval was refused",
                                },
                                Err(_) => Resolution::Denied {
                                    outcome: ApprovalOutcome::Unavailable,
                                    detail: "the approver went away",
                                },
                            },
                        },
                        Some(limit) => tokio::select! {
                            biased;
                            () = self.ports.input.interrupted() => Resolution::Interrupted,
                            resolution = tokio::time::timeout(limit, answer) => match resolution {
                                Ok(Ok(true)) => Resolution::Allowed,
                                Ok(Ok(false)) => Resolution::Denied {
                                    outcome: ApprovalOutcome::Rejected,
                                    detail: "the approval was refused",
                                },
                                Ok(Err(_recv)) => Resolution::Denied {
                                    outcome: ApprovalOutcome::Unavailable,
                                    detail: "the approver went away",
                                },
                                Err(_elapsed) => Resolution::Denied {
                                    outcome: ApprovalOutcome::Unavailable,
                                    detail: "the approval timed out",
                                },
                            },
                        },
                    };
                    let outcome = resolution.outcome();
                    self.outlets.sink.event(&Event::ApprovalResolved {
                        at: Timestamp::now(),
                        call_id: call.id.clone(),
                        outcome,
                    });
                    self.ports.extensions.observe(&Notice::Decided {
                        call: &call,
                        outcome,
                    });
                    match resolution {
                        Resolution::Interrupted => {
                            self.transition(Machine::interrupted);
                        }
                        Resolution::Allowed => {
                            self.machine.tool_started(&call.id);
                            self.state = Waiting::AwaitingTool { call };
                        }
                        Resolution::Denied { detail, .. } => {
                            self.deny_tool(
                                call,
                                Denied {
                                    by: "approval",
                                    reason: detail.into(),
                                },
                            );
                        }
                    }
                }
                Waiting::Reacting => unreachable!("泵完动作后必然摆好等待点或收场"),
            }
        }
        self.flush_session().await?;
        match self.machine {
            Machine::Closed(turn) => Ok(turn),
            _ => unreachable!("循环只在机器收场后退出"),
        }
    }

    /// 机器动作 → 落盘 / 事件上报 / 等待目标；工具调用在这里就地决议。
    async fn pump_actions(&mut self) -> Result<(), AgentError> {
        while let Some(action) = self.machine.poll_actions() {
            match action {
                Action::Report(event) => {
                    // 先入账、再上报：模型可见的东西一定已经在日志里。
                    self.record(&event).await?;
                    self.ports.extensions.observe(&Notice::Core(&event));
                    self.outlets.sink.event(&Event::Recorded {
                        at: Timestamp::now(),
                        event,
                    });
                }
                Action::CallModel => {
                    // 提交边界：请求发出前，日志已经耐久。
                    self.flush_session().await?;
                    let request = self.ports.requests.build(self.machine.context());
                    self.state = Waiting::AwaitingModel { request };
                }
                Action::RunTool(call) => self.dispatch_tool(call),
            }
        }
        Ok(())
    }

    /// 报告类动作落进会话日志（尽力而为；`flush` 才承诺持久）。
    async fn record(&mut self, event: &agnes_core::event::Event) -> Result<(), AgentError> {
        if let Some(session) = self.outlets.session.as_mut() {
            session.append(Timestamp::now(), event).await?;
        }
        Ok(())
    }

    /// 让日志耐久；没有接会话时是空操作。
    async fn flush_session(&mut self) -> Result<(), AgentError> {
        if let Some(session) = self.outlets.session.as_mut() {
            session.flush().await?;
        }
        Ok(())
    }

    /// 门禁折叠 → 放行 / 审批挂起 / 合成拒绝。
    fn dispatch_tool(&mut self, call: ToolCall) {
        match self.ports.extensions.gate(&call) {
            Gate::Allow => {
                self.machine.tool_started(&call.id);
                self.state = Waiting::AwaitingTool { call };
            }
            Gate::Deny(denied) => self.deny_tool(call, denied),
            Gate::Ask => {
                let Some(channel) = self.ports.approvals else {
                    // 没接审批口 = 没有答覆者 = 拒绝（fail-closed）。
                    self.deny_tool(
                        call,
                        Denied {
                            by: "approval",
                            reason: "tool requires an approval, but no approval channel is wired"
                                .into(),
                        },
                    );
                    return;
                };
                let (reply, answer) = tokio::sync::oneshot::channel::<bool>();
                let request = ApprovalRequest {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    reply,
                };
                if let Err(closed) = channel.send(request) {
                    self.deny_tool(
                        call,
                        Denied {
                            by: "approval",
                            reason: format!("the approval channel was closed: {closed}"),
                        },
                    );
                    return;
                }
                self.ports
                    .extensions
                    .observe(&Notice::Asked { call: &call });
                self.outlets.sink.event(&Event::ApprovalRequested {
                    at: Timestamp::now(),
                    call: call.clone(),
                });
                self.state = Waiting::AwaitingApproval {
                    call,
                    answer,
                    timeout: channel.timeout(),
                };
            }
        }
    }

    /// 以一次合成拒绝推进机器：先让插件观察、再上报宿主，随后 tool_start
    /// + 失败结果落位 —— 模型看得见拒绝并自行调整。
    fn deny_tool(&mut self, call: ToolCall, denied: Denied) {
        self.ports.extensions.observe(&Notice::Gated {
            call: &call,
            denied: &denied,
        });
        self.outlets.sink.event(&Event::ToolGated {
            at: Timestamp::now(),
            call: call.clone(),
            denied: denied.clone(),
        });
        self.machine.tool_started(&call.id);
        let output = ToolOutput::error(format!("denied by {}: {}", denied.by, denied.reason));
        self.transition(|machine| machine.tool_finished(call.id, output));
    }
}

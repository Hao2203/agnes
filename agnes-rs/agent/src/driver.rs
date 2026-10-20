//! 宿主驱动状态机：把 core 的 typestate 与宿主的 IO / 策略接起来。
//!
//! 机器产出的动作在 [`Driver::pump_actions`] 里**就地**处理：事件动作先
//! 记入日志、再给扩展内核观察、最后上报宿主；`CallModel` 从日志组装请求；
//! `RunTool` 走门禁折叠（放行 / 审批 / 拒绝）。没有事件队列与指令队列 ——
//! 类型化钩子在调用点直接给出决议，系统就地执行。
//!
//! **等待点**只有「在等什么」：等模型响应 / 等工具执行 / 等审批答覆。泵完
//! 动作后，等待点作为返回值交给循环局部消费。机器与等待点都是按值流转的
//! 局部量，不挂在 `self` 上，因此转移不需要占位。三个等待点都用 `select!`
//! 与打断赛跑；审批落定后就地执行或合成拒绝。
//!
//! core 的封闭枚举不因插件扩展：拒绝的合成结果、问询的构造与挂起、结果
//! 的替换都由驱动完成，机器只看到普通的工具往返。

use std::time::Duration;

use agnes_core::llm::{LlmRequest, LlmResponse};
use agnes_core::message::{Message, ToolCall, ToolCallId, ToolOutput};
use agnes_core::turn::{
    Action, AwaitingMessage, AwaitingTools, Closed, MessageReplied, ToolsAdvanced, Turn,
};
use agnes_io::input::Input as InputPort;
use agnes_io::llm::Llm;
use agnes_io::session::{HostEvent, SessionLog};
use agnes_io::tool::ToolRegistry;
use jiff::Timestamp;
use tokio::sync::oneshot::Receiver as AnswerReceiver;

use crate::approval::{ApprovalChannel, ApprovalRequest};
use crate::ext::{ApprovalOutcome, Denied, Extensions, Gate, Notice};
use crate::log::Log;
use crate::{AgentError, Event, RequestBuilder, Sink};

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
}

impl Machine {
    fn poll_actions(&mut self) -> Option<Action> {
        match self {
            Machine::AwaitingMessage(turn) => turn.poll_actions(),
            Machine::AwaitingTools(turn) => turn.poll_actions(),
            Machine::Closed(turn) => turn.poll_actions(),
        }
    }

    fn inject(&mut self, message: Message) {
        match self {
            Machine::AwaitingMessage(turn) => turn.inject(message),
            Machine::AwaitingTools(turn) => turn.inject(message),
            Machine::Closed(_) => {} // 收场后的注入不进机器
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
                MessageReplied::Continued { turn } => Machine::AwaitingMessage(turn),
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
        }
    }
}

/// 驱动在等什么。
enum Waiting {
    /// 等模型响应。
    Model { request: LlmRequest },
    /// 等工具执行结果（已放行）。
    Tool { call: ToolCall },
    /// 等审批答覆（可选超时）。
    Approval {
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
    /// 会话日志 —— core 不持有它；驱动把机器动作里的事件按序记入。
    log: &'s mut Log,
}

impl<'a, 's, 'q> Driver<'a, 's, 'q> {
    pub(crate) fn new(log: &'s mut Log, ports: Ports<'a>, outlets: Outlets<'s, 'q>) -> Self {
        Self {
            ports,
            outlets,
            log,
        }
    }

    /// 一路驱动到收场；中止时把原因写进审计记录，再原样返回错误。
    pub(crate) async fn run(
        mut self,
        turn: Turn<AwaitingMessage>,
    ) -> Result<Turn<Closed>, AgentError> {
        match self.drive(Machine::AwaitingMessage(turn)).await {
            Ok(closed) => Ok(closed),
            Err(err) => {
                self.record_failure(&err).await;
                Err(err)
            }
        }
    }

    async fn drive(&mut self, mut machine: Machine) -> Result<Turn<Closed>, AgentError> {
        loop {
            // 输入：运行中的消息随时进机器缓存；生效时机由机器按 step
            // 边界决定。Input 端口只承载用户输入，其余来源由 core 的
            // `inject(Message)` 直接表达。
            for content in self.ports.input.drain() {
                machine.inject(Message::User { content });
            }
            let (next, wait) = self.pump_actions(machine).await?;
            let Some(wait) = wait else {
                return match next {
                    Machine::Closed(turn) => {
                        self.flush_session().await?;
                        Ok(turn)
                    }
                    _ => unreachable!("泵完动作后必然摆好等待点或收场"),
                };
            };
            machine = self.settle(next, wait).await?;
        }
    }

    /// 处理一个等待点，返回它完成后的机器。
    async fn settle(&mut self, machine: Machine, wait: Waiting) -> Result<Machine, AgentError> {
        match wait {
            Waiting::Model { request } => {
                let raced = tokio::select! {
                    biased;
                    () = self.ports.input.interrupted() => None,
                    response = self.ports.llm.complete(&request) => Some(response),
                };
                match raced {
                    None => Ok(machine.interrupted()),
                    Some(Ok(response)) => Ok(machine.model_responded(response)),
                    Some(Err(err)) => Err(AgentError::Llm(err)),
                }
            }
            Waiting::Tool { call } => self.execute(machine, call).await,
            Waiting::Approval {
                call,
                answer,
                timeout,
            } => match self.resolve_approval(&call, answer, timeout).await? {
                Resolution::Interrupted => Ok(machine.interrupted()),
                Resolution::Allowed => {
                    let mut machine = machine;
                    machine.tool_started(&call.id);
                    // 执行前先把 ToolExecutionStart 排出记账；这里只会产生记录动作。
                    let (machine, _) = self.pump_actions(machine).await?;
                    self.execute(machine, call).await
                }
                Resolution::Denied { detail, .. } => Ok(self.deny_tool(
                    machine,
                    call,
                    Denied {
                        by: "approval",
                        reason: detail.into(),
                    },
                )),
            },
        }
    }

    /// 执行一个已放行的工具调用：结果经折叠后落位；打断则补位收场。
    async fn execute(&mut self, machine: Machine, call: ToolCall) -> Result<Machine, AgentError> {
        let raced = tokio::select! {
            biased;
            () = self.ports.input.interrupted() => None,
            output = self.ports.registry.execute(&call) => Some(output),
        };
        let Some(output) = raced else {
            return Ok(machine.interrupted());
        };
        // 结果折叠：插件可以在入账前挡回，反馈仍是一条错误结果 ——
        // 调用/结果始终一一对应。
        let output = if let Some(blocked) = self.ports.extensions.settle(&call, &output) {
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
        Ok(machine.tool_finished(call.id, output))
    }

    /// 把中止原因写进审计记录。写失败不能掩盖真正的错误。
    async fn record_failure(&mut self, err: &AgentError) {
        let Some(session) = self.outlets.session.as_mut() else {
            return;
        };
        let note = HostEvent::Failed {
            cause: err.to_string(),
        };
        let written = session.append_host(Timestamp::now(), &note).await;
        if written.is_ok() {
            let _ = session.flush().await;
        }
        debug_assert!(written.is_ok(), "记录中止原因时写盘失败");
    }

    /// 追加一条宿主审计记录（审批问询/决定）。与 core 记录同样先落盘再上报。
    async fn record_host(&mut self, event: HostEvent) -> Result<(), AgentError> {
        if let Some(session) = self.outlets.session.as_mut() {
            session.append_host(Timestamp::now(), &event).await?;
        }
        Ok(())
    }

    /// 机器动作 → 落盘 / 事件上报 / 等待目标；工具调用在这里就地决议。
    /// 返回推进后的机器与它正在等的目标（收场时为 None）。
    async fn pump_actions(
        &mut self,
        mut machine: Machine,
    ) -> Result<(Machine, Option<Waiting>), AgentError> {
        let mut wait = None;
        while let Some(action) = machine.poll_actions() {
            match action {
                Action::Record(event) => {
                    self.log.record(event);
                    let event = self.log.events().last().expect("刚刚记入的事件");
                    // 先入账、再上报：模型可见的东西一定已经在日志里。
                    if let Some(session) = self.outlets.session.as_mut() {
                        session.append(Timestamp::now(), event).await?;
                    }
                    self.ports.extensions.observe(&Notice::Core(event));
                    self.outlets.sink.event(&Event::Recorded {
                        at: Timestamp::now(),
                        event: event.clone(),
                    });
                }
                Action::CallModel => {
                    // 提交边界：请求发出前，日志已经耐久。
                    self.flush_session().await?;
                    let request = self.ports.requests.build(self.log);
                    wait = Some(Waiting::Model { request });
                }
                Action::RunTool(call) => {
                    let (next, waiting) = self.dispatch_tool(machine, call).await?;
                    machine = next;
                    if let Some(waiting) = waiting {
                        wait = Some(waiting);
                    }
                }
            }
        }
        Ok((machine, wait))
    }

    /// 让日志耐久；没有接会话时是空操作。
    async fn flush_session(&mut self) -> Result<(), AgentError> {
        if let Some(session) = self.outlets.session.as_mut() {
            session.flush().await?;
        }
        Ok(())
    }

    /// 门禁折叠 → 放行 / 审批挂起 / 合成拒绝。审批问询与回绝都先落审计。
    async fn dispatch_tool(
        &mut self,
        mut machine: Machine,
        call: ToolCall,
    ) -> Result<(Machine, Option<Waiting>), AgentError> {
        match self.ports.extensions.gate(&call) {
            Gate::Allow => {
                machine.tool_started(&call.id);
                Ok((machine, Some(Waiting::Tool { call })))
            }
            Gate::Deny(denied) => Ok((self.deny_tool(machine, call, denied), None)),
            Gate::Ask => {
                let Some(channel) = self.ports.approvals else {
                    // 没接审批口 = 没有答覆者 = 拒绝（fail-closed）。
                    self.record_host(HostEvent::ApprovalResolved {
                        call_id: call.id.clone(),
                        outcome: ApprovalOutcome::Unavailable,
                    })
                    .await?;
                    let machine = self.deny_tool(
                        machine,
                        call,
                        Denied {
                            by: "approval",
                            reason: "tool requires an approval, but no approval channel is wired"
                                .into(),
                        },
                    );
                    return Ok((machine, None));
                };
                let (reply, answer) = tokio::sync::oneshot::channel::<bool>();
                let request = ApprovalRequest {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    reply,
                };
                if let Err(closed) = channel.send(request) {
                    self.record_host(HostEvent::ApprovalResolved {
                        call_id: call.id.clone(),
                        outcome: ApprovalOutcome::Unavailable,
                    })
                    .await?;
                    let machine = self.deny_tool(
                        machine,
                        call,
                        Denied {
                            by: "approval",
                            reason: format!("the approval channel was closed: {closed}"),
                        },
                    );
                    return Ok((machine, None));
                }
                self.record_host(HostEvent::ApprovalRequested {
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                })
                .await?;
                self.ports
                    .extensions
                    .observe(&Notice::Asked { call: &call });
                self.outlets.sink.event(&Event::ApprovalRequested {
                    at: Timestamp::now(),
                    call: call.clone(),
                });
                let waiting = Waiting::Approval {
                    call,
                    answer,
                    timeout: channel.timeout(),
                };
                Ok((machine, Some(waiting)))
            }
        }
    }

    /// 等待审批答覆并落定：写审计、上报、观察。
    async fn resolve_approval(
        &mut self,
        call: &ToolCall,
        answer: AnswerReceiver<bool>,
        timeout: Option<Duration>,
    ) -> Result<Resolution, AgentError> {
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
        self.record_host(HostEvent::ApprovalResolved {
            call_id: call.id.clone(),
            outcome,
        })
        .await?;
        self.outlets.sink.event(&Event::ApprovalResolved {
            at: Timestamp::now(),
            call_id: call.id.clone(),
            outcome,
        });
        self.ports
            .extensions
            .observe(&Notice::Decided { call, outcome });
        Ok(resolution)
    }

    /// 以一次合成拒绝推进机器：先让插件观察、再上报宿主，随后 tool_start
    /// + 失败结果落位 —— 模型看得见拒绝并自行调整。
    fn deny_tool(&mut self, mut machine: Machine, call: ToolCall, denied: Denied) -> Machine {
        self.ports.extensions.observe(&Notice::Gated {
            call: &call,
            denied: &denied,
        });
        self.outlets.sink.event(&Event::ToolGated {
            at: Timestamp::now(),
            call: call.clone(),
            denied: denied.clone(),
        });
        machine.tool_started(&call.id);
        let output = ToolOutput::error(format!("denied by {}: {}", denied.by, denied.reason));
        machine.tool_finished(call.id, output)
    }
}

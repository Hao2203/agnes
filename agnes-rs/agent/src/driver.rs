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

const APPROVAL_DENIER: &str = "approval";

use std::time::Duration;

use agnes_core::llm::{LlmRequest, LlmResponse};
use agnes_core::message::{Message, ToolCall, ToolCallId, ToolOutput};
use agnes_core::turn::{
    Action, AwaitingMessage, AwaitingTools, Closed, MessageReplied, ToolsAdvanced, Turn,
};
use agnes_io::input::Input as InputPort;
use agnes_io::llm::Llm;
use agnes_io::session::{HostEvent, SessionError, SessionLog};
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
enum StagedTurn {
    AwaitingMessage(Turn<AwaitingMessage>),
    AwaitingTools(Turn<AwaitingTools>),
    Closed(Turn<Closed>),
}

impl StagedTurn {
    fn poll_actions(&mut self) -> Option<Action> {
        match self {
            StagedTurn::AwaitingMessage(turn) => turn.poll_actions(),
            StagedTurn::AwaitingTools(turn) => turn.poll_actions(),
            StagedTurn::Closed(turn) => turn.poll_actions(),
        }
    }

    fn inject(&mut self, message: Message) {
        match self {
            StagedTurn::AwaitingMessage(turn) => turn.inject(message),
            StagedTurn::AwaitingTools(turn) => turn.inject(message),
            StagedTurn::Closed(_) => {
                debug_assert!(false, "a closed turn cannot accept injected messages");
            }
        }
    }

    fn tool_started(&mut self) {
        match self {
            StagedTurn::AwaitingTools(turn) => turn.tool_started(),
            _ => debug_assert!(false, "只有等工具的机器能开始执行调用"),
        }
    }

    fn model_responded(self, response: LlmResponse) -> Self {
        match self {
            StagedTurn::AwaitingMessage(turn) => match turn.model_responded(response) {
                MessageReplied::Tools { turn } => StagedTurn::AwaitingTools(turn),
                MessageReplied::Continued { turn } => StagedTurn::AwaitingMessage(turn),
                MessageReplied::Closed { turn } => StagedTurn::Closed(turn),
            },
            other => {
                debug_assert!(false, "等工具/已收场的机器不接模型响应");
                other
            }
        }
    }

    fn tool_finished(self, call_id: ToolCallId, output: ToolOutput) -> Self {
        match self {
            StagedTurn::AwaitingTools(turn) => match turn.tool_finished(call_id, output) {
                ToolsAdvanced::Ongoing { turn } => StagedTurn::AwaitingTools(turn),
                ToolsAdvanced::NextStep { turn } => StagedTurn::AwaitingMessage(turn),
                ToolsAdvanced::Closed { turn } => StagedTurn::Closed(turn),
            },
            other => {
                debug_assert!(false, "等消息/已收场的机器不接工具结果");
                other
            }
        }
    }

    fn interrupted(self) -> Self {
        match self {
            StagedTurn::AwaitingMessage(turn) => StagedTurn::Closed(turn.interrupted()),
            StagedTurn::AwaitingTools(turn) => StagedTurn::Closed(turn.interrupted()),
            closed @ StagedTurn::Closed(_) => closed,
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

/// 泵完动作后的三种合法落点。
enum Pumped {
    /// 动作已排空，但既没有等待点也没有收场 —— 审批放行后、执行工具前。
    Idle(StagedTurn),
    /// 机器摆好了一个等待点。
    Waiting(StagedTurn, Waiting),
    /// 机器已经收场。
    Closed(Turn<Closed>),
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
        match self.drive(StagedTurn::AwaitingMessage(turn)).await {
            Ok(closed) => Ok(closed),
            Err(err) => match self.record_failure(&err).await {
                Ok(()) => Err(err),
                Err(audit) => Err(AgentError::Audit {
                    cause: Box::new(err),
                    audit,
                }),
            },
        }
    }

    async fn drive(&mut self, mut staged: StagedTurn) -> Result<Turn<Closed>, AgentError> {
        loop {
            // 输入：运行中的消息随时进机器缓存；生效时机由机器按 step
            // 边界决定。收场后不再 drain —— 留在 Input 队列里的消息属于
            // 下一次 run，不能被读出来丢掉。
            if !matches!(&staged, StagedTurn::Closed(_)) {
                for content in self.ports.input.drain() {
                    staged.inject(Message::User { content });
                }
            }
            match self.pump_actions(staged).await? {
                Pumped::Closed(turn) => {
                    self.flush_session().await?;
                    return Ok(turn);
                }
                Pumped::Waiting(next, wait) => staged = self.advance(next, wait).await?,
                Pumped::Idle(_) => {
                    return Err(AgentError::Invariant {
                        reason: "the driver stalled without a wait or a closed turn",
                    });
                }
            }
        }
    }

    /// 处理一个等待点，返回它完成后的机器。
    async fn advance(
        &mut self,
        machine: StagedTurn,
        wait: Waiting,
    ) -> Result<StagedTurn, AgentError> {
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
                    machine.tool_started();
                    // 执行前先把 ToolExecutionStart 排出记账。这里应当只
                    // 产生记录动作；任何新的等待点都是驱动不变量被破坏。
                    match self.pump_actions(machine).await? {
                        Pumped::Idle(machine) => self.execute(machine, call).await,
                        _ => Err(AgentError::Invariant {
                            reason: "approval resolution produced an unexpected wait",
                        }),
                    }
                }
                Resolution::Denied { detail, .. } => Ok(self.deny_tool(
                    machine,
                    call,
                    Denied {
                        by: APPROVAL_DENIER,
                        reason: detail.into(),
                    },
                )),
            },
        }
    }

    /// 执行一个已放行的工具调用：结果经折叠后落位；打断则补位收场。
    async fn execute(
        &mut self,
        machine: StagedTurn,
        call: ToolCall,
    ) -> Result<StagedTurn, AgentError> {
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

    /// 把中止原因写进审计记录。写失败不能掩盖真正的错误：调用方把
    /// 审计错误作为 [`AgentError::Audit`] 的一部分返回。
    async fn record_failure(&mut self, err: &AgentError) -> Result<(), SessionError> {
        let Some(session) = self.outlets.session.as_mut() else {
            return Ok(());
        };
        let note = HostEvent::Failed {
            cause: err.to_string(),
        };
        session.append_host(Timestamp::now(), &note).await?;
        session.flush().await
    }

    /// 追加一条宿主审计记录（审批问询/决定）。与 core 记录同样先落盘再上报。
    async fn record_host(&mut self, event: HostEvent) -> Result<(), AgentError> {
        if let Some(session) = self.outlets.session.as_mut() {
            session.append_host(Timestamp::now(), &event).await?;
        }
        Ok(())
    }

    /// 机器动作 → 落盘 / 事件上报 / 等待目标；工具调用在这里就地决议。
    async fn pump_actions(&mut self, mut staged: StagedTurn) -> Result<Pumped, AgentError> {
        let mut wait = None;
        while let Some(action) = staged.poll_actions() {
            match action {
                Action::Record(event) => {
                    let at = Timestamp::now();
                    let event = self.log.record(event);
                    // 先入账、再上报：模型可见的东西一定已经在日志里。
                    if let Some(session) = self.outlets.session.as_mut() {
                        session.append(at, event).await?;
                    }
                    self.ports.extensions.observe(&Notice::Core(event));
                    self.outlets.sink.event(&Event::Recorded {
                        at,
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
                    let (next, waiting) = self.dispatch_tool(staged, call).await?;
                    staged = next;
                    if let Some(waiting) = waiting {
                        wait = Some(waiting);
                    }
                }
            }
        }
        Ok(match wait {
            Some(wait) => Pumped::Waiting(staged, wait),
            None => match staged {
                StagedTurn::Closed(turn) => Pumped::Closed(turn),
                other => Pumped::Idle(other),
            },
        })
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
        mut machine: StagedTurn,
        call: ToolCall,
    ) -> Result<(StagedTurn, Option<Waiting>), AgentError> {
        match self.ports.extensions.gate(&call) {
            Gate::Allow => {
                machine.tool_started();
                Ok((machine, Some(Waiting::Tool { call })))
            }
            Gate::Deny(denied) => Ok((self.deny_tool(machine, call, denied), None)),
            Gate::Ask => {
                let Some(channel) = self.ports.approvals else {
                    // 没接审批口 = 没有答覆者 = 拒绝（fail-closed）。
                    return self
                        .deny_unanswered(
                            machine,
                            call,
                            "tool requires an approval, but no approval channel is wired".into(),
                        )
                        .await;
                };
                // 先记问询再发送：审计里不会出现“没有问询的落定”。
                self.record_host(HostEvent::ApprovalRequested {
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                })
                .await?;
                let (reply, answer) = tokio::sync::oneshot::channel::<bool>();
                let request = ApprovalRequest {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    reply,
                };
                if let Err(err) = channel.send(request) {
                    return self
                        .deny_unanswered(
                            machine,
                            call,
                            format!("the approval channel is unavailable: {err}"),
                        )
                        .await;
                }
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

    /// 没有答覆者时的统一落定：记 `Resolved{Unavailable}`、合成拒绝。
    async fn deny_unanswered(
        &mut self,
        machine: StagedTurn,
        call: ToolCall,
        reason: String,
    ) -> Result<(StagedTurn, Option<Waiting>), AgentError> {
        self.record_host(HostEvent::ApprovalResolved {
            call_id: call.id.clone(),
            outcome: ApprovalOutcome::Unavailable,
        })
        .await?;
        Ok((
            self.deny_tool(
                machine,
                call,
                Denied {
                    by: APPROVAL_DENIER,
                    reason,
                },
            ),
            None,
        ))
    }

    /// 等待审批答覆并落定：写审计、上报、观察。
    async fn resolve_approval(
        &mut self,
        call: &ToolCall,
        answer: AnswerReceiver<bool>,
        timeout: Option<Duration>,
    ) -> Result<Resolution, AgentError> {
        // 把“可选超时”归一化成一个 future，三种答覆只映射一次。
        let answer = async {
            match timeout {
                Some(limit) => tokio::time::timeout(limit, answer).await.ok(),
                None => Some(answer.await),
            }
        };
        let resolution = tokio::select! {
            biased;
            () = self.ports.input.interrupted() => Resolution::Interrupted,
            answer = answer => match answer {
                None => Resolution::Denied {
                    outcome: ApprovalOutcome::Unavailable,
                    detail: "the approval timed out",
                },
                Some(Ok(true)) => Resolution::Allowed,
                Some(Ok(false)) => Resolution::Denied {
                    outcome: ApprovalOutcome::Rejected,
                    detail: "the approval was refused",
                },
                Some(Err(_recv)) => Resolution::Denied {
                    outcome: ApprovalOutcome::Unavailable,
                    detail: "the approver went away",
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
    fn deny_tool(&mut self, mut machine: StagedTurn, call: ToolCall, denied: Denied) -> StagedTurn {
        self.ports.extensions.observe(&Notice::Gated {
            call: &call,
            denied: &denied,
        });
        self.outlets.sink.event(&Event::ToolGated {
            at: Timestamp::now(),
            call: call.clone(),
            denied: denied.clone(),
        });
        machine.tool_started();
        let output = ToolOutput::error(format!("denied by {}: {}", denied.by, denied.reason));
        machine.tool_finished(call.id, output)
    }
}

//! 宿主驱动状态机：把 core 的 typestate 与宿主的 IO / 策略接起来。
//!
//! 机器产出的动作在 [`Driver::pump_actions`] 里**就地**处理：事件动作先
//! 记入日志、再分发给插件、最后上报宿主；`CallModel` 从日志组装请求；
//! `RunTool` 在门禁检查点取用插件的指令。
//!
//! 插件只有一条处理入口（`on_event`）：它看不到派发/执行/审批这些阶段，
//! 只接收事件、返回指令。内核把指令收进 outbox，系统在检查点执行：
//!
//! - 门禁：`Dispatch` 前取 `Deny` / `RequireApproval`；
//! - 结果：`ToolCallFinished` 后取 `ReplaceResult`；
//! - 注入：每次发布事件后排空注入队列，交给 core 的注入缓冲；
//! - 中止：在下一个 `CallModel` / `RunTool` 前取 `Abort`；
//! - 工具变更：在 `CallModel` 前应用，并记录一条新 `Header`。
//!
//! **等待点**只有「在等什么」：等模型响应 / 等工具执行 / 等审批答覆。泵完
//! 动作后，等待点作为返回值交给循环局部消费。机器与等待点都是按值流转的
//! 局部量，不挂在 `self` 上，因此转移不需要占位。三个等待点都用 `select!`
//! 与打断赛跑；审批落定后就地执行或合成拒绝。
//!
//! core 的封闭枚举不因插件扩展：拒绝的合成结果、问询的构造与挂起、结果
//! 的替换、注入的落地都由驱动完成，机器只看到普通的工具往返。

const APPROVAL_DENIER: &str = "approval";

use std::time::Duration;

use agnes_core::header::{Environment, Header};
use agnes_core::message::{Message, ToolCall, ToolOutput};
use agnes_core::turn::{Action, AwaitingMessage, Closed, Turn};
use agnes_io::session::{HostEvent, SessionError};
use jiff::Timestamp;
use tokio::sync::oneshot::Receiver as AnswerReceiver;

use crate::approval::ApprovalRequest;
use crate::ext::{Aborted, ApprovalOutcome, Denied, Gate, PluginEvent};
use crate::log::Log;
use crate::{AgentError, Event};

mod state;

pub(crate) use state::{Outlets, Ports};
use state::{Pumped, Resolution, StagedTurn, Waiting};

/// 驱动状态机。
pub(crate) struct Driver<'a, 's, 'q> {
    ports: Ports<'a>,
    outlets: Outlets<'s, 'q>,
    /// 会话日志 —— core 不持有它；驱动把机器动作里的事件按序记入。
    log: &'s mut Log,
    /// 本次 run 被插件中止时的原因。
    aborted: Option<Aborted>,
}

impl<'a, 's, 'q> Driver<'a, 's, 'q> {
    pub(crate) fn new(log: &'s mut Log, ports: Ports<'a>, outlets: Outlets<'s, 'q>) -> Self {
        Self {
            ports,
            outlets,
            log,
            aborted: None,
        }
    }

    /// 一路驱动到收场；中止时把原因写进审计记录，再原样返回错误。
    pub(crate) async fn run(
        mut self,
        turn: Turn<AwaitingMessage>,
    ) -> Result<(Turn<Closed>, Option<Aborted>), AgentError> {
        match self.drive(StagedTurn::AwaitingMessage(turn)).await {
            Ok(closed) => Ok((closed, self.aborted.take())),
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
            if staged.is_open() {
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
                    Some(Ok(response)) => {
                        let mut machine = machine;
                        // 注入在模型回复落账前进入缓冲：答复后的续跑判断
                        // 与下一个 step 边界都能看到它。
                        self.drain_injections(&mut machine);
                        Ok(machine.model_responded(response))
                    }
                    Some(Err(err)) => Err(AgentError::Llm(err)),
                }
            }
            Waiting::Tool { call } => self.execute(machine, call).await,
            Waiting::Approval {
                call,
                answer,
                timeout,
            } => {
                let mut machine = machine;
                match self
                    .resolve_approval(&mut machine, &call, answer, timeout)
                    .await?
                {
                    Resolution::Interrupted => Ok(machine.interrupted()),
                    Resolution::Allowed => {
                        // 审批落定本身也会产生事件；插件可能已经在中止。
                        if self.take_abort(&machine).await?.is_some() {
                            return Ok(machine.interrupted());
                        }
                        self.drain_injections(&mut machine);
                        // 第二次门禁：只认新的拒绝；Ask 已经问过，不再重问。
                        match self.ports.extensions.gate(&call.id) {
                            Gate::Deny(denied) => Ok(self.deny_tool(machine, call, denied)),
                            Gate::Allow | Gate::Ask => {
                                machine.tool_started();
                                // 执行前先把 ToolExecutionStart 排出记账。这里
                                // 应当只产生记录动作；任何新的等待点都是驱动
                                // 不变量被破坏。
                                match self.pump_actions(machine).await? {
                                    Pumped::Idle(machine) => self.execute(machine, call).await,
                                    _ => Err(AgentError::Invariant {
                                        reason: "approval resolution produced an unexpected wait",
                                    }),
                                }
                            }
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
                }
            }
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
        let mut machine = machine;
        // 先让插件看到原始结果；注入在这里排空，保证 step 边界生效。
        self.publish(
            &mut machine,
            &PluginEvent::ToolCallFinished {
                call: &call,
                output: &output,
            },
        );
        // 结果折叠：插件可以在入账前挡回，反馈仍是一条错误结果 ——
        // 调用/结果始终一一对应。
        let output = if let Some(blocked) = self.ports.extensions.replacement(&call.id) {
            self.publish(
                &mut machine,
                &PluginEvent::ToolResultBlocked {
                    call: &call,
                    by: blocked.by,
                    feedback: &blocked.feedback,
                },
            );
            ToolOutput::error(blocked.feedback)
        } else {
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

    /// 追加一条宿主审计记录（审批问询/决定、插件中止）。与 core 记录
    /// 同样先落盘再上报。
    async fn record_host(&mut self, event: HostEvent) -> Result<(), AgentError> {
        if let Some(session) = self.outlets.session.as_mut() {
            session.append_host(Timestamp::now(), &event).await?;
        }
        Ok(())
    }

    /// 审批落定：写审计、上报宿主；分发单独的 [`publish`] 负责。
    async fn audit_approval_resolved(
        &mut self,
        call: &ToolCall,
        outcome: ApprovalOutcome,
    ) -> Result<(), AgentError> {
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
        Ok(())
    }

    /// 把一条事件分发给插件，并把注入队列排进机器。
    fn publish(&mut self, staged: &mut StagedTurn, event: &PluginEvent<'_>) {
        self.ports.extensions.publish(event);
        self.drain_injections(staged);
    }

    /// 排空注入队列：插件只入队，系统在这里统一交给 core。
    fn drain_injections(&mut self, staged: &mut StagedTurn) {
        if !staged.is_open() {
            return;
        }
        while let Some(queued) = self.ports.extensions.take_injection() {
            staged.inject(Message::User {
                content: queued.into_content(),
            });
        }
    }

    /// 取走一条中止指令；已经收场的 turn 不覆盖既有结果。
    async fn take_abort(&mut self, staged: &StagedTurn) -> Result<Option<Aborted>, AgentError> {
        if !staged.is_open() {
            let _ = self.ports.extensions.abort();
            return Ok(None);
        }
        let Some(aborted) = self.ports.extensions.abort() else {
            return Ok(None);
        };
        self.record_host(HostEvent::Aborted {
            by: aborted.by.to_owned(),
            reason: aborted.reason.clone(),
        })
        .await?;
        self.outlets.sink.event(&Event::Aborted {
            at: Timestamp::now(),
            by: aborted.by,
            reason: aborted.reason.clone(),
        });
        self.aborted = Some(aborted.clone());
        Ok(Some(aborted))
    }

    /// 应用本轮工具变更：先写候选注册表，成功插入 header 后才原子替换。
    fn apply_tool_changes(&mut self, staged: &mut StagedTurn) -> Result<(), AgentError> {
        // 没有待发调用：变更留在 outbox，收场路径会应用给下一 turn。
        if !staged.is_about_to_call_model() {
            return Ok(());
        }
        let changes = self.ports.extensions.take_tool_changes();
        if changes.is_empty() {
            return Ok(());
        }
        let mut candidate = self.ports.registry.clone();
        let mut changed = false;
        for change in changes {
            if change.apply(&mut candidate) {
                changed = true;
            }
        }
        // 重名 Add / 缺失 Remove 都是 no-op：不记录无意义的 header。
        if !changed {
            return Ok(());
        }
        let Some(system) = self.log.header().map(|header| header.system().clone()) else {
            return Err(AgentError::Invariant {
                reason: "tool changes applied before the turn header was recorded",
            });
        };
        let header = Header::new(system, Environment::new(candidate.specs()));
        if !staged.set_environment(header) {
            debug_assert!(false, "tool changes applied without a pending model call");
            return Ok(());
        }
        *self.ports.registry = candidate;
        Ok(())
    }

    /// turn 收场时把剩下的工具变更应用到注册表，供下一 turn 使用；
    /// 已收场的 turn 不再补记 header。
    fn apply_leftover_tool_changes(&mut self) {
        for change in self.ports.extensions.take_tool_changes() {
            change.apply(self.ports.registry);
        }
    }

    /// 机器动作 → 落盘 / 事件上报 / 等待目标；检查点在动作之间执行。
    async fn pump_actions(&mut self, mut staged: StagedTurn) -> Result<Pumped, AgentError> {
        let mut wait = None;
        let mut reconfigured = false;
        loop {
            if staged.is_about_to_work() {
                // 中止优先：不再启动新的模型调用或工具派发。
                if self.take_abort(&staged).await?.is_some() {
                    staged = staged.interrupted();
                    reconfigured = false;
                    continue;
                }
                // 每次模型调用前最多一轮工具变更；由新 header 事件再产生
                // 的变更留到下一次调用。
                if !reconfigured
                    && staged.is_about_to_call_model()
                    && self.ports.extensions.has_tool_changes()
                {
                    self.apply_tool_changes(&mut staged)?;
                    reconfigured = true;
                    continue;
                }
            }
            let Some(action) = staged.poll_actions() else {
                break;
            };
            if matches!(&action, Action::CallModel) {
                reconfigured = false;
            }
            match action {
                Action::Record(event) => {
                    let at = Timestamp::now();
                    // 先入账、再分发、再上报：模型可见的东西一定已经在
                    // 日志里。克隆一次，供审计/插件/上报共用。
                    let event = self.log.record(event).clone();
                    if let Some(session) = self.outlets.session.as_mut() {
                        session.append(at, &event).await?;
                    }
                    self.publish(&mut staged, &PluginEvent::Core(&event));
                    self.outlets.sink.event(&Event::Recorded { at, event });
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
                StagedTurn::Closed(turn) => {
                    self.apply_leftover_tool_changes();
                    // 已收场的 turn 不再执行迟到的定向指令；丢弃，避免
                    // 污染下一次 run。
                    self.ports.extensions.clear_pending_commands();
                    Pumped::Closed(turn)
                }
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

    /// 门禁检查点：发布 `ToolCallRequested`，然后取用指令。
    async fn dispatch_tool(
        &mut self,
        mut machine: StagedTurn,
        call: ToolCall,
    ) -> Result<(StagedTurn, Option<Waiting>), AgentError> {
        self.publish(
            &mut machine,
            &PluginEvent::ToolCallRequested { call: &call },
        );
        // 在 `ToolCallRequested` 上发出的中止必须在派发前生效。
        if self.take_abort(&machine).await?.is_some() {
            return Ok((machine.interrupted(), None));
        }
        match self.ports.extensions.gate(&call.id) {
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
                self.publish(
                    &mut machine,
                    &PluginEvent::ApprovalRequested { call: &call },
                );
                self.outlets.sink.event(&Event::ApprovalRequested {
                    at: Timestamp::now(),
                    call: call.clone(),
                });
                // 问询发布后立刻检查中止：不为一个已经注定中止的 run 等待。
                if self.take_abort(&machine).await?.is_some() {
                    self.audit_approval_resolved(&call, ApprovalOutcome::Cancelled)
                        .await?;
                    self.publish(
                        &mut machine,
                        &PluginEvent::ApprovalResolved {
                            call: &call,
                            outcome: ApprovalOutcome::Cancelled,
                        },
                    );
                    return Ok((machine.interrupted(), None));
                }
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

    /// 等待审批答覆并落定：写审计、上报、分发。
    async fn resolve_approval(
        &mut self,
        machine: &mut StagedTurn,
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
        self.audit_approval_resolved(call, outcome).await?;
        self.publish(machine, &PluginEvent::ApprovalResolved { call, outcome });
        Ok(resolution)
    }

    /// 以一次合成拒绝推进机器：先让插件看到、再上报宿主，随后 tool_start
    /// + 失败结果落位 —— 模型看得见拒绝并自行调整。
    fn deny_tool(&mut self, mut machine: StagedTurn, call: ToolCall, denied: Denied) -> StagedTurn {
        self.publish(
            &mut machine,
            &PluginEvent::ToolCallGated {
                call: &call,
                denied: &denied,
            },
        );
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

//! 宿主驱动：把 core 的 typestate 与宿主的 IO / 策略接起来。
//!
//! 阶段即动作：[`Agent<Ready>`] / [`Agent<ReadyWithGoal>`] 表示可以开始，
//! [`Agent<AwaitingCall>`] 表示该调模型，[`Agent<AwaitingModel>`] 表示等响应，
//! [`Agent<AwaitingTools>`] 表示该跑这批调用。驱动记录完开局事件后才 `start`，
//! 随后在待调用与等工具两个循环边界之间推进；记录事件由驱动自己铸造
//! （记入日志、分发给插件、上报宿主），core 不产出事件。
//!
//! 插件只有一条处理入口（`on_event`）：它看不到派发/执行/审批这些阶段，
//! 只接收事件、返回指令。内核把指令收进 outbox，系统在检查点执行：
//!
//! - 门禁：`ToolCallRequested` 前取 `Deny` / `RequireApproval`；
//! - 结果：`ToolCallFinished` 后取 `ReplaceResult`；
//! - 注入：只在待调用阶段排空，构建请求前并入会话；
//! - 中止：在每次模型调用 / 每个工具调用前取 `Abort`；
//! - 工具变更：在模型调用前应用，并记录一条新 `Header`。
//!
//! 模型与每个工具的等待都用 `select!` 与打断赛跑；审批落定后就地执行或
//! 合成拒绝。core 的封闭枚举不因插件扩展：拒绝的合成结果、问询的构造与
//! 挂起、结果的替换、注入的落地都由驱动完成，机器只看到普通的工具往返。

use std::time::Duration;

use agnes_core::agent::{
    Advanced, Agent, AwaitingCall, AwaitingModel, AwaitingTools, Closed, Ready, ReadyWithGoal,
    Replied,
};
use agnes_core::content::Content;
use agnes_core::header::{Environment, Header};
use agnes_core::llm::StopKind;
use agnes_core::message::{Message, ToolCall, ToolOutput};
use agnes_core::turn::{Outcome, StepNumber, Turn};
use jiff::Timestamp;
use tokio::sync::oneshot::Receiver as AnswerReceiver;

use crate::approval::{ApprovalOutcome, ApprovalRequest};
use crate::ext::{Aborted, Denied, Gate, Origin, PluginEvent};
use crate::log::Log;
use crate::record::{Event as RecordEvent, HostEvent, StepOutcome};
use crate::session::SessionError;
use crate::{AgentError, Event};

mod state;

use state::Resolution;
pub(crate) use state::{Outlets, Ports};

const APPROVAL_DENIER: &str = "approval";

/// 门禁与审批的落点。
enum GateOutcome {
    /// 放行，可以执行。
    Allowed,
    /// 拒绝，按合成失败结果推进机器。
    Denied(Denied),
    /// 等待被打断 —— 整个 run 收场。
    Interrupted,
}

/// 驱动侧动态阶段：机器在运行时以枚举持有，阶段本身提示下一步动作。
enum Staged {
    AwaitingCall(Agent<AwaitingCall>),
    AwaitingTools(Agent<AwaitingTools>),
    Closed(Agent<Closed>),
}

/// 一次工具结果落位后的去向。
enum Settled {
    Ongoing(Agent<AwaitingTools>),
    Next(Agent<AwaitingCall>),
    Closed(Agent<Closed>),
}

/// 驱动入口的两种开局：无 goal / 带 goal。
pub(crate) enum Start {
    Empty(Agent<Ready>),
    Goal(Agent<ReadyWithGoal>),
}

impl Start {
    fn turn(&self) -> &Turn {
        match self {
            Self::Empty(agent) => agent.turn(),
            Self::Goal(agent) => agent.turn(),
        }
    }

    fn opening_goal(&self) -> Option<&Content> {
        match self {
            Self::Empty(_) => None,
            Self::Goal(agent) => Some(agent.goal()),
        }
    }

    fn into_started(self) -> Agent<AwaitingCall> {
        match self {
            Self::Empty(agent) => agent.start(),
            Self::Goal(agent) => agent.start(),
        }
    }
}

/// 驱动状态机。
pub(crate) struct Driver<'a, 's, 'q> {
    ports: Ports<'a>,
    outlets: Outlets<'s, 'q>,
    /// 会话日志 —— core 不持有它；驱动把事件按序记入。
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
        start: Start,
    ) -> Result<(Agent<Closed>, Option<Aborted>), AgentError> {
        let outcome = match self.record_start(&start).await {
            Ok(()) => self.drive(start.into_started()).await,
            Err(err) => Err(err),
        };
        match outcome {
            Ok(closed) => {
                // 收场时把剩下的工具变更应用到注册表，供下一 turn 使用。
                self.apply_leftover_tool_changes();
                // 已收场的 turn 不再执行迟到的定向指令；丢弃。
                self.ports.extensions.clear_pending_commands();
                self.flush_session().await?;
                Ok((closed, self.aborted.take()))
            }
            Err(err) => match self.record_failure(&err).await {
                Ok(()) => Err(err),
                Err(audit) => Err(AgentError::Audit {
                    cause: Box::new(err),
                    audit,
                }),
            },
        }
    }

    async fn drive(&mut self, started: Agent<AwaitingCall>) -> Result<Agent<Closed>, AgentError> {
        let mut staged = Staged::AwaitingCall(started);
        loop {
            staged = match staged {
                Staged::AwaitingCall(agent) => self.call_model(agent).await?,
                Staged::AwaitingTools(agent) => self.run_tools(agent).await?,
                Staged::Closed(agent) => return Ok(agent),
            };
        }
    }

    /// turn 开局的记录：header、（可选）goal 消息、turn 边界。goal 从
    /// 就绪阶段读取，记录完成后才并入 `Turn`。此时还不能注入，插件指令
    /// 留在队列里。
    async fn record_start(&mut self, start: &Start) -> Result<(), AgentError> {
        let header = start.turn().header().clone();
        self.record(RecordEvent::Header(header)).await?;
        if let Some(goal) = start.opening_goal() {
            let goal = goal.clone();
            self.record(RecordEvent::Message(Message::User { content: goal }))
                .await?;
        }
        self.record(RecordEvent::TurnStart).await
    }

    /// step 的起点：先 `StepStart`，再 `MessageStart`。
    async fn record_step_start(&mut self, step: StepNumber) -> Result<(), AgentError> {
        self.record(RecordEvent::StepStart { step }).await?;
        self.record(RecordEvent::MessageStart { step }).await
    }

    /// 一次模型往返：注入 → 工具变更 → 请求快照 → call → 响应落账。
    async fn call_model(&mut self, mut agent: Agent<AwaitingCall>) -> Result<Staged, AgentError> {
        // 注入立即进会话；先入账，再检查中止，保证日志里的消息不少。
        self.drain_and_record(&mut agent).await?;

        // 中止优先：不再启动新的模型调用。
        if self.take_abort().await?.is_some() {
            return self.close_interrupted_call(agent).await;
        }
        // 工具变更与新 header 必须在请求构建前落账：请求用到的环境与
        // 日志里的最后一条 header 一致。
        if self.ports.extensions.has_tool_changes() {
            if let Some(event) = self.apply_tool_changes(&mut agent) {
                self.record(event).await?;
            }
            // header 已经分发；插件可能借此注入或中止。
            self.drain_and_record(&mut agent).await?;
            if self.take_abort().await?.is_some() {
                return self.close_interrupted_call(agent).await;
            }
        }

        let request = self.ports.requests.build(agent.turn());
        let model = request.model.clone();
        let agent = agent.call();
        let step = agent.step();
        self.record_step_start(step).await?;
        // step 的事件也可能触发中止；请求尚未发出，收场仍然干净。
        if self.take_abort().await?.is_some() {
            return self.close_interrupted_model(agent).await;
        }

        // 提交边界：请求发出前，日志已经耐久。
        self.flush_session().await?;
        let raced = tokio::select! {
            biased;
            () = self.ports.input.interrupted() => None,
            response = self.ports.llm.complete(&request) => Some(response),
        };
        let Some(response) = raced else {
            return self.close_interrupted_model(agent).await;
        };
        let response = response.map_err(AgentError::Llm)?;

        let stop = StopKind::from(&response.stop_reason);
        let usage = response.usage;
        let step_outcome = StepOutcome::from(stop);
        let before = agent.turn().messages().len();

        match agent.respond(response) {
            Replied::Tools(agent) => {
                self.record_messages(&agent, before).await?;
                self.record(RecordEvent::MessageEnd {
                    step,
                    model,
                    stop,
                    usage,
                })
                .await?;
                Ok(Staged::AwaitingTools(agent))
            }
            Replied::Closed(agent) => {
                self.record_messages(&agent, before).await?;
                self.record(RecordEvent::MessageEnd {
                    step,
                    model,
                    stop,
                    usage,
                })
                .await?;
                self.record(RecordEvent::StepEnd {
                    step,
                    outcome: step_outcome,
                })
                .await?;
                self.record(RecordEvent::TurnEnd {
                    outcome: agent.outcome(),
                })
                .await?;
                Ok(Staged::Closed(agent))
            }
        }
    }

    /// 执行模型交出的整批调用：逐个门禁、审批、执行，结果按 id 落位。
    async fn run_tools(&mut self, mut agent: Agent<AwaitingTools>) -> Result<Staged, AgentError> {
        let step = agent.step();
        loop {
            let Some(call) = agent.next_unfinished().cloned() else {
                return Err(AgentError::Invariant {
                    reason: "a tool phase always has an unfinished call",
                });
            };

            // 中止优先：不再启动新的工具派发。
            if self.take_abort().await?.is_some() {
                return self.close_interrupted_tools(agent).await;
            }
            self.publish(&PluginEvent::ToolCallRequested { call: &call });
            // 在 `ToolCallRequested` 上发出的中止必须在派发前生效。
            if self.take_abort().await?.is_some() {
                return self.close_interrupted_tools(agent).await;
            }

            match self.gate_call(&call).await? {
                GateOutcome::Interrupted => return self.close_interrupted_tools(agent).await,
                GateOutcome::Denied(denied) => {
                    self.publish_gated(&call, &denied);
                    self.record(RecordEvent::ToolExecutionStart {
                        step,
                        call: call.clone(),
                    })
                    .await?;
                    let output =
                        ToolOutput::error(format!("denied by {}: {}", denied.by, denied.reason));
                    match self.settle_tool(agent, &call, output, step).await? {
                        Settled::Ongoing(next) => agent = next,
                        Settled::Next(agent) => return Ok(Staged::AwaitingCall(agent)),
                        Settled::Closed(agent) => return Ok(Staged::Closed(agent)),
                    }
                }
                GateOutcome::Allowed => {
                    self.record(RecordEvent::ToolExecutionStart {
                        step,
                        call: call.clone(),
                    })
                    .await?;
                    let Some(output) = self.run_tool(&call).await else {
                        return self.close_interrupted_tools(agent).await;
                    };
                    // 先让插件看到原始结果；注入留在队列里，等下一次待调用阶段。
                    self.publish(&PluginEvent::ToolCallFinished {
                        call: &call,
                        output: &output,
                    });
                    // 结果折叠：插件可以在入账前挡回，反馈仍是一条错误结果 ——
                    // 调用/结果始终一一对应。
                    let output = if let Some(blocked) = self.ports.extensions.replacement(&call.id)
                    {
                        self.publish(&PluginEvent::ToolResultBlocked {
                            call: &call,
                            blocked: &blocked,
                        });
                        ToolOutput::error(blocked.feedback)
                    } else {
                        output
                    };
                    match self.settle_tool(agent, &call, output, step).await? {
                        Settled::Ongoing(next) => agent = next,
                        Settled::Next(agent) => return Ok(Staged::AwaitingCall(agent)),
                        Settled::Closed(agent) => return Ok(Staged::Closed(agent)),
                    }
                }
            }
        }
    }

    /// 一个工具结果落位：记录结束/结果事件，按转移落点继续。
    async fn settle_tool(
        &mut self,
        agent: Agent<AwaitingTools>,
        call: &ToolCall,
        output: ToolOutput,
        step: StepNumber,
    ) -> Result<Settled, AgentError> {
        let before = agent.turn().messages().len();
        match agent.finish(call.id.clone(), output) {
            Advanced::Ongoing(agent) => {
                self.record(RecordEvent::ToolExecutionEnd {
                    step,
                    call_id: call.id.clone(),
                })
                .await?;
                self.record_messages(&agent, before).await?;
                Ok(Settled::Ongoing(agent))
            }
            Advanced::Next(agent) => {
                self.record(RecordEvent::ToolExecutionEnd {
                    step,
                    call_id: call.id.clone(),
                })
                .await?;
                self.record_messages(&agent, before).await?;
                self.record(RecordEvent::StepEnd {
                    step,
                    outcome: StepOutcome::ToolUse,
                })
                .await?;
                Ok(Settled::Next(agent))
            }
            Advanced::Closed(agent) => {
                self.record(RecordEvent::ToolExecutionEnd {
                    step,
                    call_id: call.id.clone(),
                })
                .await?;
                self.record_messages(&agent, before).await?;
                self.record(RecordEvent::StepEnd {
                    step,
                    outcome: StepOutcome::ToolUse,
                })
                .await?;
                self.record(RecordEvent::TurnEnd {
                    outcome: agent.outcome(),
                })
                .await?;
                Ok(Settled::Closed(agent))
            }
        }
    }

    /// 调用前被打断：还没有开启 step，只需收 turn。
    async fn close_interrupted_call(
        &mut self,
        agent: Agent<AwaitingCall>,
    ) -> Result<Staged, AgentError> {
        let closed = agent.interrupted();
        self.record(RecordEvent::TurnEnd {
            outcome: Outcome::Interrupted,
        })
        .await?;
        Ok(Staged::Closed(closed))
    }

    /// 模型等待被打断：step 与 turn 都以 `Interrupted` 收场。
    async fn close_interrupted_model(
        &mut self,
        agent: Agent<AwaitingModel>,
    ) -> Result<Staged, AgentError> {
        let step = agent.step();
        let closed = agent.interrupted();
        self.record(RecordEvent::StepEnd {
            step,
            outcome: StepOutcome::Interrupted,
        })
        .await?;
        self.record(RecordEvent::TurnEnd {
            outcome: Outcome::Interrupted,
        })
        .await?;
        Ok(Staged::Closed(closed))
    }

    /// 工具等待被打断：未完成调用的结束/合成结果逐条记录，再收场。
    async fn close_interrupted_tools(
        &mut self,
        agent: Agent<AwaitingTools>,
    ) -> Result<Staged, AgentError> {
        let step = agent.step();
        let before = agent.turn().messages().len();
        let pending: Vec<ToolCall> = agent.unfinished().cloned().collect();
        let closed = agent.interrupted();
        let synthetic = new_messages(&closed, before);
        for (call, message) in pending.iter().zip(synthetic) {
            self.record(RecordEvent::ToolExecutionEnd {
                step,
                call_id: call.id.clone(),
            })
            .await?;
            self.record(RecordEvent::Message(message)).await?;
        }
        self.record(RecordEvent::StepEnd {
            step,
            outcome: StepOutcome::Interrupted,
        })
        .await?;
        self.record(RecordEvent::TurnEnd {
            outcome: Outcome::Interrupted,
        })
        .await?;
        Ok(Staged::Closed(closed))
    }

    /// 门禁与审批：放行、拒绝或被打断。审批等待与打断在这里赛跑。
    async fn gate_call(&mut self, call: &ToolCall) -> Result<GateOutcome, AgentError> {
        match self.ports.extensions.gate(&call.id) {
            Gate::Allow => Ok(GateOutcome::Allowed),
            Gate::Deny(denied) => Ok(GateOutcome::Denied(denied)),
            Gate::Ask => {
                let Some(channel) = self.ports.approvals else {
                    // 没接审批口 = 没有答覆者 = 拒绝（fail-closed）。
                    let denied = self
                        .deny_unanswered(
                            call,
                            "tool requires an approval, but no approval channel is wired".into(),
                        )
                        .await?;
                    return Ok(GateOutcome::Denied(denied));
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
                    let denied = self
                        .deny_unanswered(
                            call,
                            format!("the approval channel is unavailable: {err}"),
                        )
                        .await?;
                    return Ok(GateOutcome::Denied(denied));
                }
                self.publish(&PluginEvent::ApprovalRequested { call });
                self.outlets.sink.event(&Event::ApprovalRequested {
                    at: Timestamp::now(),
                    call: call.clone(),
                });
                // 问询发布后立刻检查中止：不为一个已经注定中止的 run 等待。
                if self.take_abort().await?.is_some() {
                    self.audit_approval_resolved(call, ApprovalOutcome::Cancelled)
                        .await?;
                    self.publish(&PluginEvent::ApprovalResolved {
                        call,
                        outcome: ApprovalOutcome::Cancelled,
                    });
                    return Ok(GateOutcome::Interrupted);
                }
                match self
                    .resolve_approval(call, answer, channel.timeout())
                    .await?
                {
                    Resolution::Interrupted => Ok(GateOutcome::Interrupted),
                    Resolution::Denied { detail, .. } => Ok(GateOutcome::Denied(Denied {
                        by: Origin::new(APPROVAL_DENIER),
                        reason: detail.into(),
                    })),
                    Resolution::Allowed => {
                        // 审批落定本身也会产生事件；插件可能已经在中止。
                        if self.take_abort().await?.is_some() {
                            return Ok(GateOutcome::Interrupted);
                        }
                        // 第二次门禁：只认新的拒绝；Ask 已经问过，不再重问。
                        match self.ports.extensions.gate(&call.id) {
                            Gate::Deny(denied) => Ok(GateOutcome::Denied(denied)),
                            Gate::Allow | Gate::Ask => Ok(GateOutcome::Allowed),
                        }
                    }
                }
            }
        }
    }

    /// 执行一个已放行的调用；打断返回 None。
    async fn run_tool(&mut self, call: &ToolCall) -> Option<ToolOutput> {
        tokio::select! {
            biased;
            () = self.ports.input.interrupted() => None,
            output = self.ports.registry.execute(call) => Some(output),
        }
    }

    /// 催化工具拒绝的宿主事件（发布给插件 + 上报）。
    fn publish_gated(&mut self, call: &ToolCall, denied: &Denied) {
        self.publish(&PluginEvent::ToolCallGated { call, denied });
        self.outlets.sink.event(&Event::ToolGated {
            at: Timestamp::now(),
            call: call.clone(),
            denied: denied.clone(),
        });
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

    /// 追加一条宿主审计记录（审批问询/决定、插件中止）。
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

    /// 把一条事件分发给插件；注入只入队，等下一次待调用阶段。
    fn publish(&mut self, event: &PluginEvent<'_>) {
        self.ports.extensions.publish(event);
    }

    /// 排空插件注入队列，立即并入会话。
    fn drain_injections(&mut self, agent: &mut Agent<AwaitingCall>) {
        while let Some(queued) = self.ports.extensions.take_injection() {
            agent.inject(queued.into_message());
        }
    }

    /// 把输入队列里的用户消息收进会话。
    fn drain_input(&mut self, agent: &mut Agent<AwaitingCall>) {
        for content in self.ports.input.drain() {
            agent.inject(Message::User { content });
        }
    }

    /// 排空输入与插件注入，并立即把新增消息记入日志。
    async fn drain_and_record(
        &mut self,
        agent: &mut Agent<AwaitingCall>,
    ) -> Result<(), AgentError> {
        let before = agent.turn().messages().len();
        self.drain_input(agent);
        self.drain_injections(agent);
        let injected = new_messages(agent, before);
        self.record_injected(&injected).await
    }

    /// 取走一条中止指令。
    async fn take_abort(&mut self) -> Result<Option<Aborted>, AgentError> {
        let Some(aborted) = self.ports.extensions.abort() else {
            return Ok(None);
        };
        self.record_host(HostEvent::Aborted {
            by: aborted.by.clone(),
            reason: aborted.reason.clone(),
        })
        .await?;
        self.outlets.sink.event(&Event::Aborted {
            at: Timestamp::now(),
            by: aborted.by.clone(),
            reason: aborted.reason.clone(),
        });
        self.aborted = Some(aborted.clone());
        Ok(Some(aborted))
    }

    /// 应用本轮工具变更：先写候选注册表，成功后才原子替换并让机器的
    /// 环境同步；只有真正变化时才返回一条 `Header` 事件。
    fn apply_tool_changes(&mut self, agent: &mut Agent<AwaitingCall>) -> Option<RecordEvent> {
        let changes = self.ports.extensions.take_tool_changes();
        if changes.is_empty() {
            return None;
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
            return None;
        }
        let system = agent.turn().header().system().clone();
        let environment = Environment::new(candidate.specs());
        agent.set_environment(environment.clone());
        *self.ports.registry = candidate;
        Some(RecordEvent::Header(Header::new(system, environment)))
    }

    /// turn 收场时把剩下的工具变更应用到注册表，供下一 turn 使用。
    fn apply_leftover_tool_changes(&mut self) {
        for change in self.ports.extensions.take_tool_changes() {
            change.apply(self.ports.registry);
        }
    }

    /// 一条事件的三步落地：先入日志、再分发插件、最后上报。
    async fn record(&mut self, event: RecordEvent) -> Result<(), AgentError> {
        let at = Timestamp::now();
        // 克隆一次，供审计/插件/上报共用。
        let event = self.log.record(event).clone();
        if let Some(session) = self.outlets.session.as_mut() {
            session.append(at, &event).await?;
        }
        self.ports.extensions.publish(&PluginEvent::Record(&event));
        self.outlets.sink.event(&Event::Recorded { at, event });
        Ok(())
    }

    /// 记录自 `before` 起新增的会话消息。
    async fn record_messages<Phase>(
        &mut self,
        agent: &Agent<Phase>,
        before: usize,
    ) -> Result<(), AgentError> {
        for message in new_messages(agent, before) {
            self.record(RecordEvent::Message(message)).await?;
        }
        Ok(())
    }

    /// 记录调用前生效的注入消息：先记数量，再逐条入账。
    async fn record_injected(&mut self, injected: &[Message]) -> Result<(), AgentError> {
        if injected.is_empty() {
            return Ok(());
        }
        self.record(RecordEvent::MessagesInjected {
            count: injected.len(),
        })
        .await?;
        for message in injected {
            self.record(RecordEvent::Message(message.clone())).await?;
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

    /// 没有答覆者时的统一落定：记 `Resolved{Unavailable}`、给出拒绝。
    async fn deny_unanswered(
        &mut self,
        call: &ToolCall,
        reason: String,
    ) -> Result<Denied, AgentError> {
        self.record_host(HostEvent::ApprovalResolved {
            call_id: call.id.clone(),
            outcome: ApprovalOutcome::Unavailable,
        })
        .await?;
        Ok(Denied {
            by: Origin::new(APPROVAL_DENIER),
            reason,
        })
    }

    /// 等待审批答覆并落定：写审计、上报、分发。
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
        self.audit_approval_resolved(call, outcome).await?;
        self.publish(&PluginEvent::ApprovalResolved { call, outcome });
        Ok(resolution)
    }
}

/// 自 `before` 起新增的会话消息（借看；调用方决定怎么处理）。
fn new_messages<Phase>(agent: &Agent<Phase>, before: usize) -> Vec<Message> {
    agent.turn().messages()[before..].to_vec()
}

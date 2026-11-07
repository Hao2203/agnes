//! 消息管线：一组系统按注册顺序解释 [`agnes_core::agent`] 的阶段转移。
//!
//! 每个 turn 请求是一个实体：提交时挂 [`TurnRequest`] + [`TurnQueued`]，
//! 调度时在**同一实体**上插入 typestate 阶段与时钟，收场时写
//! [`TurnResponse`]。响应不再经过消息总线，读者缺失或调度提前返回都不会
//! 让它消失。终态先由各路径写成 [`TurnTerminal`]，再由
//! [`finalize_requests`] 统一落成响应（恰好一次）。
//!
//! turn 实体身上的 [`TurnOwner`] 回指所属 agent 实体，日志与耐久都按 owner
//! 路由，多个 agent 的管线互不串扰。
//!
//! 错误分级：系统返回 `Err` 仅用于不变量被破坏（缺资源）—— 它会短路整个
//! 调度；运行期可恢复错误（模型失败、会话失败、缺 provider、任务派发失败）
//! 一律转为终态响应，返回 `Ok`。

use std::collections::HashSet;
use std::time::Instant;

use agnes_core::agent::{Advanced, Agent, Replied};
use agnes_core::llm::StopKind;
use agnes_core::message::{Message, ToolCall, ToolOutput};
use agnes_core::tool::ToolName;
use agnes_core::turn::{AgentConfig, Outcome, StepNumber, Turn, TurnResult};
use agnes_ecs::{
    CancelToken, Commands, Entity, MessageReader, MessageWriter, Query, Res, ResMut, With,
};
use agnes_io::http::TransportError;
use agnes_io::llm::{LlmCall, LlmCancel, LlmError, LlmProvider, LlmReply};
use agnes_io::tool::ToolRegistry;
use agnes_plugin_async::AsyncExecutor;
use agnes_record::{
    ApprovalOutcome, Durability, Event as RecordEvent, RotateSession, RunEvent, RunEventKind,
    StepOutcome,
};
use jiff::Timestamp;

use super::RequestBuilderResource;
use super::messages::{
    ApprovalDecided, CancelTurn, InterruptTurn, ToolFinished, TurnFailure, TurnFailureKind,
    TurnRequested, UserMessage,
};
use super::request::{
    TurnQueued, TurnRequest, TurnRequestId, TurnResponded, TurnResponder, TurnResponse,
    TurnTerminal,
};
use super::state::{
    ActiveTurn, CallPhase, InFlightCall, LlmCallSeq, Staged, ToolCallEntry, TurnClock, TurnOwner,
    TurnPhase,
};
use crate::approval::{ApprovalChannel, ApprovalRequest};
use crate::permission::PermissionRules;
use crate::{AgentCore, Inbox};

const POLICY_DENIER: &str = "permission";
const CORE_STATE_MISSING: &str = "agent entity has no AgentCore";
const NO_PROVIDER: &str = "no LLM provider plugin is installed";

/// 绑定一个 agent 的 core 与上报流：一条事件先入日志，再镜像到
/// [`RunEvent`]（会话落盘与渲染各自消费）。
struct Recorder<'a, 'w> {
    core: &'a mut AgentCore,
    run_events: &'a mut MessageWriter<'w, RunEvent>,
}

impl Recorder<'_, '_> {
    fn record(&mut self, agent: Entity, event: RecordEvent) {
        let at = Timestamp::now();
        let event = self.core.log_mut().record(event).clone();
        self.run_events.write(RunEvent {
            agent,
            kind: RunEventKind::Recorded { at, event },
        });
    }

    /// 自 `before` 起新增的会话消息逐条入账。
    fn record_messages(&mut self, agent: Entity, turn: &Turn, before: usize) {
        for message in new_messages(turn, before) {
            self.record(agent, RecordEvent::Message(message));
        }
    }

    /// step 与 turn 一并收场：先 `step_end`，再 `turn_end`。
    fn settle_step(
        &mut self,
        agent: Entity,
        step: StepNumber,
        step_outcome: StepOutcome,
        turn_outcome: Outcome,
    ) {
        self.record(
            agent,
            RecordEvent::StepEnd {
                step,
                outcome: step_outcome,
            },
        );
        self.record(
            agent,
            RecordEvent::TurnEnd {
                outcome: turn_outcome,
            },
        );
    }
}

/// 规则表对这把工具的 deny 命中：命中返回合成给模型的拒绝理由。
fn deny_reason(rules: Option<&PermissionRules>, name: &ToolName) -> Option<String> {
    rules.and_then(|rules| {
        rules
            .deny
            .iter()
            .find(|rule| rule.matches(name))
            .map(|_| format!("tool `{name}` is denied by policy"))
    })
}

/// 拒绝一次调用：门禁事件先落账，机器随后收到合成的失败结果。
fn deny_entry(
    run_events: &mut MessageWriter<RunEvent>,
    agent: Entity,
    entry: &mut ToolCallEntry,
    reason: String,
) {
    run_events.write(RunEvent {
        agent,
        kind: RunEventKind::ToolGated {
            at: Timestamp::now(),
            call: entry.call.clone(),
            denied: agnes_record::Denied {
                by: POLICY_DENIER.into(),
                reason: reason.clone(),
            },
        },
    });
    entry.phase = CallPhase::Denied(ToolOutput::error(reason));
}

/// 自 `before` 起新增的会话消息。
fn new_messages(turn: &Turn, before: usize) -> Vec<Message> {
    turn.messages()[before..].to_vec()
}

fn missing_core_failure() -> TurnFailure {
    TurnFailure {
        cause: CORE_STATE_MISSING.to_owned(),
        kind: TurnFailureKind::MissingCore,
    }
}

fn model_failure(cause: &str) -> TurnFailure {
    TurnFailure {
        cause: cause.to_owned(),
        kind: TurnFailureKind::Model,
    }
}

fn session_failure(cause: String) -> TurnFailure {
    TurnFailure {
        cause,
        kind: TurnFailureKind::Session,
    }
}

fn cancelled_failure(cause: &str) -> TurnFailure {
    TurnFailure {
        cause: cause.to_owned(),
        kind: TurnFailureKind::Cancelled,
    }
}

fn internal_failure(cause: &str) -> TurnFailure {
    TurnFailure {
        cause: cause.to_owned(),
        kind: TurnFailureKind::Internal,
    }
}

/// 让一个请求以失败收场：记失败事件 + 写终态意图，由
/// [`finalize_requests`] 交出 [`TurnResponse`]。不中止整个调度。
fn fail_turn(
    commands: &mut Commands,
    run_events: &mut MessageWriter<RunEvent>,
    request: Entity,
    agent: Entity,
    failure: TurnFailure,
) {
    run_events.write(RunEvent {
        agent,
        kind: RunEventKind::Failed {
            at: Timestamp::now(),
            cause: failure.cause.clone(),
        },
    });
    commands.entity(request).insert(TurnTerminal {
        result: Err(failure),
        finished_at: Timestamp::now(),
    });
}

/// 轮转会话：清空目标 agent 的内存日志。
///
/// 与 `agnes_plugin_session` 的 `session_rotate_durability` 配对，两者各自
/// 消费独立的 [`RotateSession`] 游标。注册在请求调度之前，同一 tick 内的
/// 下一个 turn 从空日志重建。
pub fn session_rotate_log(
    mut rotations: MessageReader<RotateSession>,
    mut cores: Query<&mut AgentCore>,
) {
    for rotation in rotations.read() {
        let Some(mut core) = cores.get_mut(rotation.agent) else {
            continue;
        };
        core.log_mut().reset();
    }
}

/// 把跨线程/外部客户端提交的 [`TurnRequested`] 落成请求实体。
///
/// 本系统常驻注册：请求一旦提交就进入世界，不会因为“没有人读消息”而消失。
pub fn ingest_requests(mut commands: Commands, mut requests: MessageReader<TurnRequested>) {
    for request in requests.read() {
        crate::host::submit_turn(
            &mut commands,
            request.id,
            request.agent,
            request.config.clone(),
            request.responder.clone(),
        );
    }
}

/// 调度：从排队请求里按 id 顺序为每个空闲 agent 启动最早的一条。
///
/// 同一 agent 每 tick 只启动一个请求，其余保持 `TurnQueued`；不同 agent
/// 互不阻塞。目标实体没有 [`AgentCore`] 时请求直接以失败终态收场；会话
/// 已 fail-closed 时整队以 `Session` 失败收场，不再等待。
pub fn schedule_turns(
    mut commands: Commands,
    mut cores: Query<(Entity, &mut AgentCore, Option<&Durability>)>,
    running: Query<&TurnOwner, With<ActiveTurn>>,
    queued: Query<(Entity, &TurnRequest, &TurnQueued)>,
    mut run_events: MessageWriter<RunEvent>,
) {
    let mut busy: HashSet<Entity> = running.iter().map(|owner| owner.0).collect();
    let mut pending: Vec<(Entity, TurnRequestId, Entity)> = queued
        .iter()
        .map(|(entity, request, _)| (entity, request.id, request.agent))
        .collect();
    pending.sort_unstable_by_key(|(_, id, _)| *id);

    for (entity, _, agent) in pending {
        if busy.contains(&agent) {
            continue;
        }
        let Some((_, mut core, durability)) = cores.get_mut(agent) else {
            fail_turn(
                &mut commands,
                &mut run_events,
                entity,
                agent,
                missing_core_failure(),
            );
            continue;
        };
        if let Some(cause) = durability.and_then(|durability| durability.failed.clone()) {
            fail_turn(
                &mut commands,
                &mut run_events,
                entity,
                agent,
                session_failure(cause),
            );
            continue;
        }
        let Some((_, request, _)) = queued.get(entity) else {
            continue;
        };
        let config = request.config.clone();
        let mut recorder = Recorder {
            core: &mut core,
            run_events: &mut run_events,
        };
        start_turn(&mut commands, &mut recorder, entity, agent, &config);
        busy.insert(agent);
    }
}

/// 在请求实体上启动 turn：从所属 agent 的日志重建机器、记开局事件
/// （header、goal 消息、turn 边界），并挂上运行期组件。
fn start_turn(
    commands: &mut Commands,
    recorder: &mut Recorder<'_, '_>,
    request: Entity,
    agent: Entity,
    config: &AgentConfig,
) {
    let messages: Vec<_> = recorder.core.log().messages().cloned().collect();
    let turn = Turn::with_messages(config.header.clone(), config.budget, messages);
    let started_at = Timestamp::now();
    recorder.run_events.write(RunEvent {
        agent,
        kind: RunEventKind::Started {
            at: started_at,
            goal: config.goal.clone(),
            tools: config
                .header
                .environment()
                .tools()
                .map(|spec| spec.name().clone())
                .collect(),
        },
    });
    recorder.record(agent, RecordEvent::Header(turn.header().clone()));
    let staged = match config.goal.clone() {
        Some(goal) => {
            recorder.record(
                agent,
                RecordEvent::Message(Message::User {
                    content: goal.clone(),
                }),
            );
            Staged::AwaitingCall(Agent::with_goal(turn, goal).start())
        }
        None => Staged::AwaitingCall(Agent::new(turn).start()),
    };
    recorder.record(agent, RecordEvent::TurnStart);
    commands
        .entity(request)
        .remove::<TurnQueued>()
        .insert_bundle((
            TurnPhase::new(staged),
            TurnClock {
                started_at,
                cancellation: CancelToken::new(),
            },
            TurnOwner(agent),
            ActiveTurn,
        ));
}

/// 注入排空：用户消息只在目标 agent 的待调用阶段并入会话，其余场合在该
/// agent 的 [`Inbox`] 队列里等下一个边界。与旧驱动一致：先入账，消息不少。
pub fn inject_pending(
    mut messages: MessageReader<UserMessage>,
    mut cores: Query<(Entity, &mut AgentCore)>,
    mut inboxes: Query<(Entity, &mut Inbox)>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for message in messages.read() {
        if cores.get(message.agent).is_none() {
            log::error!(
                "dropping UserMessage for agent {:?}: it has no AgentCore",
                message.agent
            );
            continue;
        }
        let Some((_, mut inbox)) = inboxes.get_mut(message.agent) else {
            log::error!(
                "dropping UserMessage for agent {:?}: it has no Inbox",
                message.agent
            );
            continue;
        };
        inbox.push_message(message.content.clone());
    }
    for (_, owner, mut phase) in phases.iter_mut() {
        let Some(Staged::AwaitingCall(turn)) = phase.0.as_mut() else {
            continue;
        };
        let Some((_, mut core)) = cores.get_mut(owner.0) else {
            continue;
        };
        let Some((_, mut inbox)) = inboxes.get_mut(owner.0) else {
            continue;
        };
        let injected_contents = inbox.take_pending_messages();
        if injected_contents.is_empty() {
            continue;
        }
        let before = turn.turn().messages().len();
        for content in &injected_contents {
            turn.inject(Message::User {
                content: content.clone(),
            });
        }
        let injected = new_messages(turn.turn(), before);
        if injected.is_empty() {
            continue;
        }
        let mut recorder = Recorder {
            core: &mut core,
            run_events: &mut run_events,
        };
        recorder.record(
            owner.0,
            RecordEvent::MessagesInjected {
                count: injected.len(),
            },
        );
        for message in injected {
            recorder.record(owner.0, RecordEvent::Message(message));
        }
    }
}

/// turn 进行中丢失核心状态：让受影响的 turn 以失败收场，不中止调度。
///
/// 注册在 `schedule_turns` 之后、其余 turn 系统之前：同 tick 内新启动的 turn
/// 也会被检查；被摘除 `AgentCore` 的 agent 不再影响其他 agent 的调度。
/// 排队请求由 `schedule_turns` 在同一 tick 收口。
pub fn fail_on_missing_core(
    mut commands: Commands,
    cores: Query<&AgentCore>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    queued: Query<(Entity, &TurnRequest, Option<&TurnTerminal>), With<TurnQueued>>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for (entity, owner, mut phase) in phases.iter_mut() {
        if phase.0.is_none() || cores.get(owner.0).is_some() {
            continue;
        }
        phase.0 = None;
        fail_turn(
            &mut commands,
            &mut run_events,
            entity,
            owner.0,
            missing_core_failure(),
        );
    }
    // 排队请求：agent 核心已丢失时整队收口（`schedule_turns` 已写过终态的
    // 请求会被跳过，避免重复失败事件）。
    for (entity, request, terminal) in queued.iter() {
        if terminal.is_some() || cores.get(request.agent).is_some() {
            continue;
        }
        fail_turn(
            &mut commands,
            &mut run_events,
            entity,
            request.agent,
            missing_core_failure(),
        );
    }
}

/// 会话失败是 fail-closed：让该 agent 的活跃 turn 以失败收场，不再发起
/// 新调用。排队请求由 `schedule_turns` 收口。
pub fn fail_on_session_failure(
    mut commands: Commands,
    sessions: Query<(Entity, &Durability), With<AgentCore>>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for (root, durability) in sessions.iter() {
        let Some(cause) = durability.failed.as_ref() else {
            continue;
        };
        for (entity, owner, mut phase) in phases.iter_mut() {
            if owner.0 != root || phase.0.is_none() {
                continue;
            }
            phase.0 = None;
            fail_turn(
                &mut commands,
                &mut run_events,
                entity,
                root,
                session_failure(cause.clone()),
            );
        }
    }
}

/// 发出一次模型调用（`LlmCall` 消息），回复经 [`LlmReply`] 回注。
///
/// 耐久门禁是旧驱动的提交边界：请求发出前，该 agent 已记录的日志必须
/// 已经落盘。没有 provider 时请求以失败终态收场，而不是让整 tick 返回
/// `Err`、把已写入的响应一起丢掉。
#[allow(clippy::too_many_arguments)]
pub fn issue_model_call(
    requests: Res<RequestBuilderResource>,
    provider: Option<Res<LlmProvider>>,
    mut seq: ResMut<LlmCallSeq>,
    mut cores: Query<(&mut AgentCore, Option<&Durability>)>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut outgoing: MessageWriter<LlmCall>,
    mut run_events: MessageWriter<RunEvent>,
    mut commands: Commands,
) {
    for (entity, owner, mut phase) in phases.iter_mut() {
        let Some((mut core, durability)) = cores.get_mut(owner.0) else {
            // 核心状态已被摘除；turn 由 fail_on_missing_core 收场。
            continue;
        };
        if provider.is_none() {
            if matches!(phase.0, Some(Staged::AwaitingCall(_))) {
                phase.0 = None;
                fail_turn(
                    &mut commands,
                    &mut run_events,
                    entity,
                    owner.0,
                    model_failure(NO_PROVIDER),
                );
            }
            continue;
        }
        if durability.is_some_and(|durability| !durability.is_durable()) {
            // 该 agent 本 tick 的记录还没落盘；等确认后再发请求。
            continue;
        }
        let staged = phase.0.take();
        match staged {
            Some(Staged::AwaitingCall(turn)) => {
                let mut recorder = Recorder {
                    core: &mut core,
                    run_events: &mut run_events,
                };
                let request = requests.build(turn.turn(), recorder.core.settings());
                let model = request.model.clone();
                let turn = turn.call();
                let step = turn.step();
                recorder.record(owner.0, RecordEvent::StepStart { step });
                recorder.record(owner.0, RecordEvent::MessageStart { step });
                let id = seq.issue();
                commands.entity(entity).insert(InFlightCall {
                    id,
                    model: model.clone(),
                });
                outgoing.write(LlmCall { id, request });
                phase.0 = Some(Staged::AwaitingModel(turn));
            }
            other => phase.0 = other,
        }
    }
}

/// 处理模型回复：按 id 找到挂着 [`InFlightCall`] 的 turn，推进机器（或落
/// 成工具调用实体），取消按打断收场，失败让 turn 失败收场（应用继续跑）。
/// 迟到的回复（turn 已收场或已被打断）直接丢弃。
pub fn process_llm_reply(
    mut commands: Commands,
    mut replies: MessageReader<LlmReply>,
    in_flight: Query<(Entity, &InFlightCall)>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut cores: Query<&mut AgentCore>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for reply in replies.read() {
        let Some((entity, call)) = in_flight.iter().find(|(_, call)| call.id == reply.id) else {
            continue;
        };
        let model = call.model.clone();
        commands.entity(entity).remove::<InFlightCall>();
        let Some((_, owner, mut phase)) = phases.iter_mut().find(|(turn, _, _)| *turn == entity)
        else {
            continue;
        };
        let agent = owner.0;
        let Some(mut core) = cores.get_mut(agent) else {
            // 核心状态已被摘除；turn 由 fail_on_missing_core 收场。
            continue;
        };
        let mut recorder = Recorder {
            core: &mut core,
            run_events: &mut run_events,
        };
        let staged = phase.0.take();
        let next = match (staged, &reply.result) {
            (Some(Staged::AwaitingModel(turn)), Ok(incoming)) => {
                let stop = StopKind::from(&incoming.stop_reason);
                let usage = incoming.usage;
                let step_outcome = StepOutcome::from(stop);
                let step = turn.step();
                let before = turn.turn().messages().len();
                match turn.respond(incoming.clone()) {
                    Replied::Tools(turn) => {
                        recorder.record_messages(agent, turn.turn(), before);
                        recorder.record(
                            agent,
                            RecordEvent::MessageEnd {
                                step,
                                model: model.clone(),
                                stop,
                                usage,
                            },
                        );
                        for (index, call) in turn.calls().enumerate() {
                            commands.spawn_bundle((
                                ToolCallEntry {
                                    turn: entity,
                                    step,
                                    index,
                                    call: call.clone(),
                                    phase: CallPhase::Pending,
                                },
                                TurnOwner(agent),
                            ));
                        }
                        Some(Staged::AwaitingTools(turn))
                    }
                    Replied::Closed(turn) => {
                        recorder.record_messages(agent, turn.turn(), before);
                        recorder.record(
                            agent,
                            RecordEvent::MessageEnd {
                                step,
                                model: model.clone(),
                                stop,
                                usage,
                            },
                        );
                        recorder.settle_step(agent, step, step_outcome, turn.outcome());
                        Some(Staged::Closed(turn))
                    }
                }
            }
            (
                Some(Staged::AwaitingModel(turn)),
                Err(LlmError::Transport(TransportError::Cancelled)),
            ) => {
                let step = turn.step();
                let closed = turn.interrupted();
                recorder.settle_step(agent, step, StepOutcome::Interrupted, Outcome::Interrupted);
                Some(Staged::Closed(closed))
            }
            (Some(Staged::AwaitingModel(_)), Err(error)) => {
                fail_turn(
                    &mut commands,
                    recorder.run_events,
                    entity,
                    agent,
                    model_failure(&error.to_string()),
                );
                None
            }
            (other, _) => other,
        };
        phase.0 = next;
    }
}

/// 门禁：规则表（deny > ask > 不表态）决定放行、拒绝或问询。没有审批口
/// 时的 ask 按 fail-closed 落定为拒绝。
pub fn gate_tool_calls(
    rules: Option<Res<PermissionRules>>,
    approvals: Option<Res<ApprovalChannel>>,
    mut entries: Query<(&mut ToolCallEntry, &TurnOwner)>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for (mut entry, owner) in entries.iter_mut() {
        if !matches!(entry.phase, CallPhase::Pending) {
            continue;
        }
        if let Some(reason) = deny_reason(rules.as_deref(), &entry.call.name) {
            deny_entry(&mut run_events, owner.0, &mut entry, reason);
            continue;
        }
        let asks = rules
            .as_ref()
            .map(|rules| rules.ask.iter().any(|rule| rule.matches(&entry.call.name)))
            .unwrap_or(false);
        if !asks {
            entry.phase = CallPhase::Allowed;
            continue;
        }
        let Some(channel) = approvals.as_ref() else {
            // 没接审批口 = 没有答覆者 = 拒绝（fail-closed）。
            run_events.write(RunEvent {
                agent: owner.0,
                kind: RunEventKind::ApprovalResolved {
                    at: Timestamp::now(),
                    call_id: entry.call.id.clone(),
                    outcome: ApprovalOutcome::Unavailable,
                },
            });
            entry.phase = CallPhase::Denied(ToolOutput::error(
                "tool requires an approval, but no approval channel is wired",
            ));
            continue;
        };
        // 先发问询事件再递送：审计里不会出现“没有问询的落定”。
        run_events.write(RunEvent {
            agent: owner.0,
            kind: RunEventKind::ApprovalRequested {
                at: Timestamp::now(),
                call: entry.call.clone(),
            },
        });
        let request = ApprovalRequest {
            agent: owner.0,
            call_id: entry.call.id.clone(),
            tool_name: entry.call.name.clone(),
            arguments: entry.call.arguments.clone(),
        };
        if let Err(err) = channel.send(request) {
            run_events.write(RunEvent {
                agent: owner.0,
                kind: RunEventKind::ApprovalResolved {
                    at: Timestamp::now(),
                    call_id: entry.call.id.clone(),
                    outcome: ApprovalOutcome::Unavailable,
                },
            });
            entry.phase = CallPhase::Denied(ToolOutput::error(format!(
                "the approval channel is unavailable: {err}"
            )));
            continue;
        }
        entry.phase = CallPhase::Asked {
            deadline: channel.timeout().map(|limit| Instant::now() + limit),
        };
    }
}

/// 审批落定：答覆与超时（level-triggered）。答覆按 `(agent, call_id)` 匹配，
/// 避免不同 agent 的同名 call id 串扰。批准后做第二次门禁 —— 只认新的
/// deny；Ask 已经问过，不再重问。
pub fn resolve_approvals(
    mut decided: MessageReader<ApprovalDecided>,
    rules: Option<Res<PermissionRules>>,
    mut entries: Query<(&mut ToolCallEntry, &TurnOwner)>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for (mut entry, owner) in entries.iter_mut() {
        let CallPhase::Asked { deadline } = &entry.phase else {
            continue;
        };
        let expired = deadline
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(false);
        if expired {
            run_events.write(RunEvent {
                agent: owner.0,
                kind: RunEventKind::ApprovalResolved {
                    at: Timestamp::now(),
                    call_id: entry.call.id.clone(),
                    outcome: ApprovalOutcome::Unavailable,
                },
            });
            entry.phase = CallPhase::Denied(ToolOutput::error("the approval timed out"));
        }
    }
    for decision in decided.read() {
        let Some((mut entry, owner)) = entries.iter_mut().find(|(entry, owner)| {
            owner.0 == decision.agent
                && entry.call.id == decision.call_id
                && matches!(entry.phase, CallPhase::Asked { .. })
        }) else {
            continue;
        };
        let agent = owner.0;
        if !decision.approved {
            run_events.write(RunEvent {
                agent,
                kind: RunEventKind::ApprovalResolved {
                    at: Timestamp::now(),
                    call_id: entry.call.id.clone(),
                    outcome: ApprovalOutcome::Rejected,
                },
            });
            entry.phase = CallPhase::Denied(ToolOutput::error("the approval was refused"));
            continue;
        }
        run_events.write(RunEvent {
            agent,
            kind: RunEventKind::ApprovalResolved {
                at: Timestamp::now(),
                call_id: entry.call.id.clone(),
                outcome: ApprovalOutcome::AllowedOnce,
            },
        });
        if let Some(reason) = deny_reason(rules.as_deref(), &entry.call.name) {
            deny_entry(&mut run_events, agent, &mut entry, reason);
            continue;
        }
        entry.phase = CallPhase::Allowed;
    }
}

/// 派发已放行的调用：记 `ToolExecutionStart`，异步任务与 turn 的取消令牌
/// 赛跑，打断返回合成的 `not_run`。派发失败转成一次失败的工具结果，不再
/// 让整个调度返回 `Err`。
pub fn execute_tools(
    executor: Res<AsyncExecutor>,
    registry: Res<ToolRegistry>,
    clocks: Query<&TurnClock>,
    owners: Query<&TurnOwner>,
    mut entries: Query<(Entity, &mut ToolCallEntry)>,
    mut cores: Query<&mut AgentCore>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for (entity, mut entry) in entries.iter_mut() {
        if !matches!(entry.phase, CallPhase::Allowed) {
            continue;
        }
        let agent = match owners.get(entry.turn) {
            Some(owner) => owner.0,
            None => {
                // turn 已收场；它的调用实体由 finalize 清理，不再派发。
                entry.phase = CallPhase::Settled;
                continue;
            }
        };
        let Some(mut core) = cores.get_mut(agent) else {
            // 核心状态已被摘除；turn 由 fail_on_missing_core 收场。
            entry.phase = CallPhase::Settled;
            continue;
        };
        {
            let mut recorder = Recorder {
                core: &mut core,
                run_events: &mut run_events,
            };
            recorder.record(
                agent,
                RecordEvent::ToolExecutionStart {
                    step: entry.step,
                    call: entry.call.clone(),
                },
            );
        }
        drop(core);
        let token = clocks
            .get(entry.turn)
            .map(|clock| clock.cancellation.clone());
        let task_registry = registry.clone();
        let call = entry.call.clone();
        let task_executor = executor.clone();
        let spawned = executor.spawn(async move {
            let output = match token {
                Some(token) => tokio::select! {
                    biased;
                    () = token.cancelled() => ToolOutput::not_run(),
                    output = task_registry.execute(&call) => output,
                },
                None => task_registry.execute(&call).await,
            };
            let _ = task_executor.send(ToolFinished {
                call: entity,
                output,
            });
        });
        match spawned {
            Ok(()) => entry.phase = CallPhase::Running,
            Err(error) => {
                entry.phase = CallPhase::Denied(ToolOutput::error(format!(
                    "failed to dispatch the tool task: {error}"
                )));
            }
        }
    }
}

/// 落位：任务回注的结果与门禁合成的拒绝都在这里推进机器。结算顺序按
/// (step, 调用位置) 确定。
pub fn finish_tool_calls(
    mut results: MessageReader<ToolFinished>,
    mut entries: Query<(Entity, &mut ToolCallEntry)>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut cores: Query<&mut AgentCore>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for message in results.read() {
        if let Some((_, mut entry)) = entries.get_mut(message.call)
            && matches!(entry.phase, CallPhase::Running)
        {
            entry.phase = CallPhase::Done(message.output.clone());
        }
    }

    let mut ready: Vec<(u32, usize, Entity)> = entries
        .iter()
        .filter(|(_, entry)| matches!(entry.phase, CallPhase::Done(_) | CallPhase::Denied(_)))
        .map(|(entity, entry)| (entry.step.get(), entry.index, entity))
        .collect();
    ready.sort_unstable();

    for (_, _, entity) in ready {
        let Some((_, mut entry)) = entries.get_mut(entity) else {
            continue;
        };
        let (step, call_id, output) = match &entry.phase {
            CallPhase::Done(output) | CallPhase::Denied(output) => {
                (entry.step, entry.call.id.clone(), output.clone())
            }
            _ => continue,
        };
        entry.phase = CallPhase::Settled;
        let turn = entry.turn;
        drop(entry);

        let Some((_, owner, mut phase)) = phases
            .iter_mut()
            .find(|(candidate, _, _)| *candidate == turn)
        else {
            // turn 已收场（被打断）：迟到结果直接丢弃。
            continue;
        };
        let agent = owner.0;
        let Some(mut core) = cores.get_mut(agent) else {
            // 核心状态已被摘除；turn 由 fail_on_missing_core 收场。
            continue;
        };
        let mut recorder = Recorder {
            core: &mut core,
            run_events: &mut run_events,
        };
        let staged = phase.0.take();
        let next = match staged {
            Some(Staged::AwaitingTools(turn)) => {
                let before = turn.turn().messages().len();
                let advanced = turn.finish(call_id.clone(), output);
                let advanced_turn = match &advanced {
                    Advanced::Ongoing(turn) => turn.turn(),
                    Advanced::Next(turn) => turn.turn(),
                    Advanced::Closed(turn) => turn.turn(),
                };
                recorder.record(
                    agent,
                    RecordEvent::ToolExecutionEnd {
                        step,
                        call_id: call_id.clone(),
                    },
                );
                recorder.record_messages(agent, advanced_turn, before);
                match advanced {
                    Advanced::Ongoing(turn) => Some(Staged::AwaitingTools(turn)),
                    Advanced::Next(turn) => {
                        recorder.record(
                            agent,
                            RecordEvent::StepEnd {
                                step,
                                outcome: StepOutcome::ToolUse,
                            },
                        );
                        Some(Staged::AwaitingCall(turn))
                    }
                    Advanced::Closed(turn) => {
                        recorder.settle_step(agent, step, StepOutcome::ToolUse, turn.outcome());
                        Some(Staged::Closed(turn))
                    }
                }
            }
            other => other,
        };
        phase.0 = next;
    }
}

/// 打断/取消收场：目标 turn 按阶段走 [`Agent::interrupted`] 的对应路径，
/// 取消令牌让 in-flight 任务停下，在飞的模型调用补发 [`LlmCancel`]；
/// 迟到的回注消息会被忽略（turn 已消失）。
///
/// - [`InterruptTurn`] 按 agent 打断当前 turn，不碰排队请求；
/// - [`CancelTurn`] 按请求 id 取消：排队中直接终态，运行中走同一条打断路径。
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn handle_turn_cancels(
    mut interrupts: MessageReader<InterruptTurn>,
    mut cancels: MessageReader<CancelTurn>,
    mut commands: Commands,
    mut requests: Query<(
        Entity,
        &TurnRequest,
        Option<&TurnQueued>,
        Option<&mut TurnPhase>,
        Option<&TurnClock>,
        Option<&mut InFlightCall>,
    )>,
    mut entries: Query<(&mut ToolCallEntry, &TurnOwner)>,
    mut cores: Query<&mut AgentCore>,
    mut run_events: MessageWriter<RunEvent>,
    mut llm_cancels: MessageWriter<LlmCancel>,
) {
    let interrupt_agents: Vec<Entity> = interrupts
        .read()
        .iter()
        .map(|message| message.agent)
        .collect();
    let cancel_ids: Vec<TurnRequestId> = cancels
        .read()
        .iter()
        .map(|message| message.request)
        .collect();
    if interrupt_agents.is_empty() && cancel_ids.is_empty() {
        return;
    }

    let mut interrupted: Vec<Entity> = Vec::new();
    for (entity, request, queued, phase, clock, in_flight) in requests.iter_mut() {
        let by_agent = interrupt_agents.contains(&request.agent);
        let by_id = cancel_ids.contains(&request.id);
        if !by_agent && !by_id {
            continue;
        }
        if queued.is_some() {
            // agent 级打断不碰排队请求；按 id 取消则直接收口。
            if by_id {
                fail_turn(
                    &mut commands,
                    &mut run_events,
                    entity,
                    request.agent,
                    cancelled_failure("the request was cancelled before it started"),
                );
            }
            continue;
        }
        let Some(mut phase) = phase else {
            continue;
        };
        if let Some(clock) = clock {
            clock.cancellation.cancel();
        }
        if let Some(call) = in_flight {
            llm_cancels.write(LlmCancel { id: call.id });
        }
        let Some(mut core) = cores.get_mut(request.agent) else {
            // 核心状态已被摘除；turn 由 fail_on_missing_core 收场。
            continue;
        };
        let agent = request.agent;
        let mut recorder = Recorder {
            core: &mut core,
            run_events: &mut run_events,
        };
        let staged = phase.0.take();
        let next = match staged {
            Some(Staged::AwaitingCall(turn)) => {
                let closed = turn.interrupted();
                recorder.record(
                    agent,
                    RecordEvent::TurnEnd {
                        outcome: Outcome::Interrupted,
                    },
                );
                Some(Staged::Closed(closed))
            }
            Some(Staged::AwaitingModel(turn)) => {
                let step = turn.step();
                let closed = turn.interrupted();
                recorder.settle_step(agent, step, StepOutcome::Interrupted, Outcome::Interrupted);
                Some(Staged::Closed(closed))
            }
            Some(Staged::AwaitingTools(turn)) => {
                let step = turn.step();
                let before = turn.turn().messages().len();
                let pending: Vec<ToolCall> = turn.unfinished().cloned().collect();
                let closed = turn.interrupted();
                let synthetic = new_messages(closed.turn(), before);
                for (call, message) in pending.iter().zip(synthetic) {
                    recorder.record(
                        agent,
                        RecordEvent::ToolExecutionEnd {
                            step,
                            call_id: call.id.clone(),
                        },
                    );
                    recorder.record(agent, RecordEvent::Message(message));
                }
                recorder.settle_step(agent, step, StepOutcome::Interrupted, Outcome::Interrupted);
                Some(Staged::Closed(closed))
            }
            other => other,
        };
        phase.0 = next;
        interrupted.push(entity);
    }
    // 挂起的审批问询按取消落定（与旧驱动的打断路径一致）。
    for (mut entry, owner) in entries.iter_mut() {
        if matches!(entry.phase, CallPhase::Asked { .. }) && interrupted.contains(&entry.turn) {
            run_events.write(RunEvent {
                agent: owner.0,
                kind: RunEventKind::ApprovalResolved {
                    at: Timestamp::now(),
                    call_id: entry.call.id.clone(),
                    outcome: ApprovalOutcome::Cancelled,
                },
            });
            entry.phase = CallPhase::Settled;
        }
    }
}

/// 收场意图：正常收场的 turn 写 [`TurnTerminal`]（携带结果）。
///
/// 失败/取消路径已经直接写了终态；本系统只处理机器走到 `Closed` 的正常
/// 收场。响应由 [`finalize_requests`] 统一落成。
pub fn close_turn(
    mut commands: Commands,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for (entity, owner, mut phase) in phases.iter_mut() {
        if !matches!(phase.0, Some(Staged::Closed(_))) {
            continue;
        }
        let Some(Staged::Closed(turn)) = phase.0.take() else {
            continue;
        };
        let result: TurnResult = turn.into_result();
        run_events.write(RunEvent {
            agent: owner.0,
            kind: RunEventKind::Finished {
                at: Timestamp::now(),
                outcome: result.outcome,
            },
        });
        commands.entity(entity).insert(TurnTerminal {
            result: Ok(result),
            finished_at: Timestamp::now(),
        });
    }
}

/// 防御性收口：阶段被取走却没有留下终态意图的请求。
///
/// 正常转移总是把阶段放回；这里兜住未来新增路径的遗漏，避免实体静默泄漏。
pub fn fail_on_dropped_phase(
    mut commands: Commands,
    phases: Query<(
        Entity,
        &TurnPhase,
        Option<&TurnTerminal>,
        Option<&TurnResponse>,
    )>,
) {
    for (entity, phase, terminal, response) in phases.iter() {
        if phase.0.is_some() || terminal.is_some() || response.is_some() {
            continue;
        }
        commands.entity(entity).insert(TurnTerminal {
            result: Err(internal_failure(
                "turn phase was dropped without a terminal state",
            )),
            finished_at: Timestamp::now(),
        });
    }
}

/// 唯一收口：把 [`TurnTerminal`] 落成 [`TurnResponse`]，恰好一次。
///
/// 同时移除运行期组件、清理工具调用实体；有 [`TurnResponder`] 时推送并
/// 标记回收，没有则保留实体供宿主拉取。
#[allow(clippy::type_complexity)]
pub fn finalize_requests(
    mut commands: Commands,
    requests: Query<(
        Entity,
        &TurnRequest,
        &TurnTerminal,
        Option<&TurnClock>,
        Option<&TurnResponder>,
        Option<&TurnResponse>,
    )>,
    calls: Query<(Entity, &ToolCallEntry)>,
) {
    for (entity, request, terminal, clock, responder, existing) in requests.iter() {
        if existing.is_some() {
            continue;
        }
        let started_at = clock.map_or(request.submitted_at, |clock| clock.started_at);
        let response = TurnResponse {
            id: request.id,
            agent: request.agent,
            result: terminal.result.clone(),
            submitted_at: request.submitted_at,
            started_at,
            finished_at: terminal.finished_at,
        };
        commands
            .entity(entity)
            .insert(response.clone())
            .remove::<TurnPhase>()
            .remove::<TurnClock>()
            .remove::<TurnOwner>()
            .remove::<ActiveTurn>()
            .remove::<InFlightCall>()
            .remove::<TurnTerminal>()
            .remove::<TurnQueued>();
        for (call_entity, entry) in calls.iter() {
            if entry.turn == entity {
                commands.entity(call_entity).despawn();
            }
        }
        if let Some(responder) = responder {
            responder.send(response);
            commands.entity(entity).insert(TurnResponded);
        }
    }
}

/// 回收已经通过 [`TurnResponder`] 送达的请求实体。
///
/// 拉取式响应不在这里回收：它必须保留到宿主消费为止，即使目标 agent 已经
/// 消失（`MissingCore` 失败响应正是这种情况）。
pub fn reap_responded_requests(
    mut commands: Commands,
    responded: Query<Entity, With<TurnResponded>>,
) {
    for entity in responded.iter() {
        commands.entity(entity).despawn();
    }
}

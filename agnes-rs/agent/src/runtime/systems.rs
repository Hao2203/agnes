//! 消息管线：一组系统按注册顺序解释 [`agnes_core::agent`] 的阶段转移。
//!
//! 每个 tick 的推进与旧驱动的检查点一一对应；事件的三步落地（日志、
//! 会话、上报）在 [`record`] 里完成 —— 会话落盘经 [`RunEvent`] 消息由
//! `agnes-plugin-session` 异步完成，耐久门禁见 [`issue_model_call`]。
//! turn 实体经 [`TurnOwner`] 回指所属 agent 根实体，日志与耐久、待处理
//! 队列都按 owner 路由，多个 agent 的管线互不串扰。
//!
//! 错误分级：系统返回 `Err` 仅用于不变量被破坏（缺资源、任务派发失败）——
//! 它会短路整个调度；运行期可恢复错误（模型失败、会话失败）一律转为
//! 失败收场事件与 [`TurnFinished`]，返回 `Ok`。

use std::sync::Arc;
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
    ApprovalOutcome, Durability, Event as RecordEvent, Log, RunEvent, RunEventKind, StepOutcome,
};
use jiff::Timestamp;

use super::RequestBuilderResource;
use super::messages::{
    ApprovalDecided, InterruptTurn, ToolFinished, TurnFailure, TurnFinished, TurnRequested,
    UserMessage,
};
use super::state::{
    ActiveTurn, CallPhase, InFlightCall, LlmCallSeq, PendingTurns, PendingUserMessages, Staged,
    ToolCallEntry, TurnClock, TurnFailed, TurnOwner, TurnPhase,
};
use crate::AgentRoot;
use crate::approval::{ApprovalChannel, ApprovalRequest};
use crate::model::ModelSettings;
use crate::permission::PermissionRules;

const POLICY_DENIER: &str = "permission";

/// 记录副手：日志与上报流总是结伴出现 —— 一条事件先入日志，再镜像到
/// 上报流。日志按 owner 路由到 turn 所属的 agent 根实体。
type Recorder<'w> = (Query<'w, &'static mut Log>, MessageWriter<'w, RunEvent>);

/// 一条事件的两步落地：先入所属 agent 的日志，再发 [`RunEvent`]（会话
/// 落盘与渲染各自消费）。
fn record(recorder: &mut Recorder, agent: Entity, event: RecordEvent) {
    let at = Timestamp::now();
    let mut log = recorder
        .0
        .get_mut(agent)
        .expect("the turn's owner has a session log");
    let event = log.record(event).clone();
    recorder.1.write(RunEvent {
        agent,
        kind: RunEventKind::Recorded { at, event },
    });
}

/// 自 `before` 起新增的会话消息逐条入账。
fn record_messages(recorder: &mut Recorder, agent: Entity, turn: &Turn, before: usize) {
    for message in new_messages(turn, before) {
        record(recorder, agent, RecordEvent::Message(message));
    }
}

/// step 与 turn 一并收场：先 `step_end`，再 `turn_end`。
fn settle_step(
    recorder: &mut Recorder,
    agent: Entity,
    step: StepNumber,
    step_outcome: StepOutcome,
    turn_outcome: Outcome,
) {
    record(
        recorder,
        agent,
        RecordEvent::StepEnd {
            step,
            outcome: step_outcome,
        },
    );
    record(
        recorder,
        agent,
        RecordEvent::TurnEnd {
            outcome: turn_outcome,
        },
    );
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

/// turn 开局：目标 agent 已有活跃 turn 就进它的队列，否则从它的日志重建
/// turn、记开局事件并启动。
///
/// 同一 agent 每 tick 只 spawn 一个 turn（spawn 是延迟应用的，本 tick 内
/// 看不到），其余请求进该 agent 的 [`PendingTurns`]，收场后依次开跑；
/// 不同 agent 的队列互不阻塞。
pub fn begin_turn(
    mut commands: Commands,
    mut requests: MessageReader<TurnRequested>,
    turns: Query<(Entity, &TurnOwner), With<ActiveTurn>>,
    mut queues: Query<(Entity, &mut PendingTurns)>,
    mut recorder: Recorder,
) {
    let mut spawned: Vec<Entity> = Vec::new();
    for request in requests.read() {
        let agent = request.agent;
        let busy = turns.iter().any(|(_, owner)| owner.0 == agent) || spawned.contains(&agent);
        if busy {
            queues
                .get_mut(agent)
                .expect("TurnRequested targets an agent root")
                .1
                .0
                .push(Arc::clone(&request));
            continue;
        }
        spawn_turn(&mut commands, &mut recorder, agent, &request.config);
        spawned.push(agent);
    }
    // 缓冲的请求：该 agent 没有活跃 turn 时开跑一个，下一个等收场后的 tick。
    for (agent, mut pending) in queues.iter_mut() {
        let busy = turns.iter().any(|(_, owner)| owner.0 == agent) || spawned.contains(&agent);
        if busy || pending.0.is_empty() {
            continue;
        }
        let request = pending.0.remove(0);
        spawn_turn(&mut commands, &mut recorder, agent, &request.config);
        spawned.push(agent);
    }
}

/// spawn 一个 turn 实体：从所属 agent 的日志重建机器、记开局事件（header、
/// goal 消息、turn 边界）并启动到待调用阶段。
fn spawn_turn(
    commands: &mut Commands,
    recorder: &mut Recorder,
    agent: Entity,
    config: &AgentConfig,
) {
    let messages: Vec<_> = recorder
        .0
        .get_mut(agent)
        .expect("the agent root has a session log")
        .messages()
        .cloned()
        .collect();
    let turn = Turn::with_messages(config.header.clone(), config.budget, messages);
    let started_at = Timestamp::now();
    recorder.1.write(RunEvent {
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
    record(recorder, agent, RecordEvent::Header(turn.header().clone()));
    let staged = match config.goal.clone() {
        Some(goal) => {
            record(
                recorder,
                agent,
                RecordEvent::Message(Message::User { content: goal }),
            );
            Staged::AwaitingCall(
                Agent::with_goal(turn, config.goal.clone().expect("goal just read")).start(),
            )
        }
        None => Staged::AwaitingCall(Agent::new(turn).start()),
    };
    record(recorder, agent, RecordEvent::TurnStart);
    commands.spawn_bundle((
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
/// agent 的 [`PendingUserMessages`] 里等下一个边界。与旧驱动一致：先入账，
/// 消息不少。
pub fn inject_pending(
    mut messages: MessageReader<UserMessage>,
    mut queues: Query<(Entity, &mut PendingUserMessages)>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut recorder: Recorder,
) {
    for message in messages.read() {
        queues
            .get_mut(message.agent)
            .expect("UserMessage targets an agent root")
            .1
            .0
            .push(message.content.clone());
    }
    for (_, owner, mut phase) in phases.iter_mut() {
        let Some(Staged::AwaitingCall(turn)) = phase.0.as_mut() else {
            continue;
        };
        let Some((_, mut queue)) = queues.get_mut(owner.0) else {
            continue;
        };
        if queue.0.is_empty() {
            continue;
        }
        let injected_contents = std::mem::take(&mut queue.0);
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
        record(
            &mut recorder,
            owner.0,
            RecordEvent::MessagesInjected {
                count: injected.len(),
            },
        );
        for message in injected {
            record(&mut recorder, owner.0, RecordEvent::Message(message));
        }
    }
}

/// 会话失败是 fail-closed：让该 agent 的活跃 turn 以失败收场，不再发起
/// 新调用。阶段丢弃（session repairs 兜底），插入 [`TurnFailed`] 标记，由
/// [`close_turn`] 发出失败结果并清理。
pub fn fail_on_session_failure(
    mut commands: Commands,
    sessions: Query<(Entity, &Durability), With<AgentRoot>>,
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
            run_events.write(RunEvent {
                agent: root,
                kind: RunEventKind::Failed {
                    at: Timestamp::now(),
                    cause: cause.clone(),
                },
            });
            phase.0 = None;
            commands.entity(entity).insert(TurnFailed {
                cause: cause.clone(),
            });
        }
    }
}

/// 启动不变量：没有 provider 插件时直接失败，而不是让调用消失在消息总线里。
pub fn require_provider(provider: Option<Res<LlmProvider>>) -> Result<(), BoxError> {
    if provider.is_none() {
        return Err(BoxError::msg(
            "no LLM provider plugin is installed; add a plugin that installs `LlmProvider`",
        ));
    }
    Ok(())
}

/// 发出一次模型调用（`LlmCall` 消息），回复经 [`LlmReply`] 回注。
///
/// 耐久门禁是旧驱动的提交边界：请求发出前，该 agent 已记录的日志必须
/// 已经落盘。在飞调用记在 turn 实体的 [`InFlightCall`] 组件上，回复与
/// 取消按 id 路由回来。
pub fn issue_model_call(
    requests: Res<RequestBuilderResource>,
    mut seq: ResMut<LlmCallSeq>,
    sessions: Query<(Entity, &ModelSettings, Option<&Durability>), With<AgentRoot>>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut recorder: Recorder,
    mut outgoing: MessageWriter<LlmCall>,
    mut commands: Commands,
) -> Result<(), BoxError> {
    for (entity, owner, mut phase) in phases.iter_mut() {
        if sessions
            .get(owner.0)
            .and_then(|(_, _, durability)| durability)
            .is_some_and(|durability| !durability.is_durable())
        {
            // 该 agent 本 tick 的记录还没落盘；等确认后再发请求。
            continue;
        }
        let staged = phase.0.take();
        let next = match staged {
            Some(Staged::AwaitingCall(turn)) => {
                let settings = sessions
                    .get(owner.0)
                    .expect("the agent root has model settings")
                    .1;
                let request = requests.build(turn.turn(), &settings);
                let model = request.model.clone();
                let turn = turn.call();
                let step = turn.step();
                record(&mut recorder, owner.0, RecordEvent::StepStart { step });
                record(&mut recorder, owner.0, RecordEvent::MessageStart { step });
                let id = seq.issue();
                commands.entity(entity).insert(InFlightCall {
                    id,
                    model: model.clone(),
                });
                outgoing.write(LlmCall { id, request });
                Some(Staged::AwaitingModel(turn))
            }
            other => other,
        };
        phase.0 = next;
    }
    Ok(())
}

/// 处理模型回复：按 id 找到挂着 [`InFlightCall`] 的 turn，推进机器（或落
/// 成工具调用实体），取消按打断收场，失败让 turn 失败收场（应用继续跑）。
/// 迟到的回复（turn 已收场或已被打断）直接丢弃。
pub fn process_llm_reply(
    mut commands: Commands,
    mut replies: MessageReader<LlmReply>,
    in_flight: Query<(Entity, &InFlightCall)>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut recorder: Recorder,
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
        let agent_root = owner.0;
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
                        record_messages(&mut recorder, agent_root, turn.turn(), before);
                        record(
                            &mut recorder,
                            agent_root,
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
                                TurnOwner(agent_root),
                            ));
                        }
                        Some(Staged::AwaitingTools(turn))
                    }
                    Replied::Closed(turn) => {
                        record_messages(&mut recorder, agent_root, turn.turn(), before);
                        record(
                            &mut recorder,
                            agent_root,
                            RecordEvent::MessageEnd {
                                step,
                                model: model.clone(),
                                stop,
                                usage,
                            },
                        );
                        settle_step(
                            &mut recorder,
                            agent_root,
                            step,
                            step_outcome,
                            turn.outcome(),
                        );
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
                settle_step(
                    &mut recorder,
                    agent_root,
                    step,
                    StepOutcome::Interrupted,
                    Outcome::Interrupted,
                );
                Some(Staged::Closed(closed))
            }
            (Some(Staged::AwaitingModel(_)), Err(error)) => {
                let cause = error.to_string();
                recorder.1.write(RunEvent {
                    agent: agent_root,
                    kind: RunEventKind::Failed {
                        at: Timestamp::now(),
                        cause: cause.clone(),
                    },
                });
                commands.entity(entity).insert(TurnFailed { cause });
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

/// 审批落定：答覆与超时（level-triggered）。批准后做第二次门禁 —— 只认
/// 新的 deny；Ask 已经问过，不再重问。
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
        let Some((mut entry, owner)) = entries.iter_mut().find(|(entry, _)| {
            entry.call.id == decision.call_id && matches!(entry.phase, CallPhase::Asked { .. })
        }) else {
            continue;
        };
        if !decision.approved {
            run_events.write(RunEvent {
                agent: owner.0,
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
            agent: owner.0,
            kind: RunEventKind::ApprovalResolved {
                at: Timestamp::now(),
                call_id: entry.call.id.clone(),
                outcome: ApprovalOutcome::AllowedOnce,
            },
        });
        if let Some(reason) = deny_reason(rules.as_deref(), &entry.call.name) {
            deny_entry(&mut run_events, owner.0, &mut entry, reason);
            continue;
        }
        entry.phase = CallPhase::Allowed;
    }
}

/// 派发已放行的调用：记 `ToolExecutionStart`，异步任务与 turn 的取消令牌
/// 赛跑，打断返回合成的 `not_run`。
pub fn execute_tools(
    executor: Res<AsyncExecutor>,
    registry: Res<ToolRegistry>,
    clocks: Query<&TurnClock>,
    owners: Query<&TurnOwner>,
    mut entries: Query<(Entity, &mut ToolCallEntry)>,
    mut recorder: Recorder,
) -> Result<(), BoxError> {
    for (entity, mut entry) in entries.iter_mut() {
        if !matches!(entry.phase, CallPhase::Allowed) {
            continue;
        }
        let agent = owners
            .get(entry.turn)
            .expect("the tool call's turn has an owner")
            .0;
        record(
            &mut recorder,
            agent,
            RecordEvent::ToolExecutionStart {
                step: entry.step,
                call: entry.call.clone(),
            },
        );
        let token = clocks
            .get(entry.turn)
            .map(|clock| clock.cancellation.clone());
        let task_registry = registry.clone();
        let call = entry.call.clone();
        let task_executor = executor.clone();
        executor
            .spawn(async move {
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
            })
            .map_err(BoxError::from)?;
        entry.phase = CallPhase::Running;
    }
    Ok(())
}

/// 落位：任务回注的结果与门禁合成的拒绝都在这里推进机器。结算顺序按
/// (step, 调用位置) 确定。
pub fn finish_tool_calls(
    mut results: MessageReader<ToolFinished>,
    mut entries: Query<(Entity, &mut ToolCallEntry)>,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    mut recorder: Recorder,
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
        let agent_root = owner.0;
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
                record(
                    &mut recorder,
                    agent_root,
                    RecordEvent::ToolExecutionEnd {
                        step,
                        call_id: call_id.clone(),
                    },
                );
                record_messages(&mut recorder, agent_root, advanced_turn, before);
                match advanced {
                    Advanced::Ongoing(turn) => Some(Staged::AwaitingTools(turn)),
                    Advanced::Next(turn) => {
                        record(
                            &mut recorder,
                            agent_root,
                            RecordEvent::StepEnd {
                                step,
                                outcome: StepOutcome::ToolUse,
                            },
                        );
                        Some(Staged::AwaitingCall(turn))
                    }
                    Advanced::Closed(turn) => {
                        settle_step(
                            &mut recorder,
                            agent_root,
                            step,
                            StepOutcome::ToolUse,
                            turn.outcome(),
                        );
                        Some(Staged::Closed(turn))
                    }
                }
            }
            other => other,
        };
        phase.0 = next;
    }
}

/// 打断收场：目标 agent 的活跃 turn 按阶段走 [`Agent::interrupted`] 的
/// 对应路径，取消令牌让 in-flight 任务停下，在飞的模型调用补发
/// [`LlmCancel`]；迟到的回注消息会被忽略（turn 已消失）。
pub fn handle_interrupt(
    mut interrupts: MessageReader<InterruptTurn>,
    mut phases: Query<(
        Entity,
        &TurnOwner,
        &mut TurnPhase,
        &TurnClock,
        Option<&mut InFlightCall>,
    )>,
    mut entries: Query<(&mut ToolCallEntry, &TurnOwner)>,
    mut recorder: Recorder,
    mut cancels: MessageWriter<LlmCancel>,
) {
    let targets: Vec<Entity> = interrupts
        .read()
        .iter()
        .map(|message| message.agent)
        .collect();
    if targets.is_empty() {
        return;
    }
    let mut interrupted: Vec<Entity> = Vec::new();
    for (entity, owner, mut phase, clock, mut in_flight) in phases.iter_mut() {
        if !targets.contains(&owner.0) {
            continue;
        }
        clock.cancellation.cancel();
        if let Some(call) = in_flight.as_deref_mut() {
            cancels.write(LlmCancel { id: call.id });
        }
        let staged = phase.0.take();
        let next = match staged {
            Some(Staged::AwaitingCall(turn)) => {
                let closed = turn.interrupted();
                record(
                    &mut recorder,
                    owner.0,
                    RecordEvent::TurnEnd {
                        outcome: Outcome::Interrupted,
                    },
                );
                Some(Staged::Closed(closed))
            }
            Some(Staged::AwaitingModel(turn)) => {
                let step = turn.step();
                let closed = turn.interrupted();
                settle_step(
                    &mut recorder,
                    owner.0,
                    step,
                    StepOutcome::Interrupted,
                    Outcome::Interrupted,
                );
                Some(Staged::Closed(closed))
            }
            Some(Staged::AwaitingTools(turn)) => {
                let step = turn.step();
                let before = turn.turn().messages().len();
                let pending: Vec<ToolCall> = turn.unfinished().cloned().collect();
                let closed = turn.interrupted();
                let synthetic = new_messages(closed.turn(), before);
                for (call, message) in pending.iter().zip(synthetic) {
                    record(
                        &mut recorder,
                        owner.0,
                        RecordEvent::ToolExecutionEnd {
                            step,
                            call_id: call.id.clone(),
                        },
                    );
                    record(&mut recorder, owner.0, RecordEvent::Message(message));
                }
                settle_step(
                    &mut recorder,
                    owner.0,
                    step,
                    StepOutcome::Interrupted,
                    Outcome::Interrupted,
                );
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
            recorder.1.write(RunEvent {
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

/// 收场清理：正常收场的 turn 发 [`TurnFinished`]，失败收场的发
/// [`TurnFinished`]（带原因）；随后 despawn turn 与它的工具调用实体。
pub fn close_turn(
    mut commands: Commands,
    mut phases: Query<(Entity, &TurnOwner, &mut TurnPhase)>,
    failures: Query<&TurnFailed>,
    clocks: Query<&TurnClock>,
    calls: Query<(Entity, &ToolCallEntry)>,
    mut run_events: MessageWriter<RunEvent>,
    mut finished: MessageWriter<TurnFinished>,
) {
    let mut closed = Vec::new();
    for (entity, owner, mut phase) in phases.iter_mut() {
        match phase.0.as_ref() {
            Some(Staged::Closed(_)) => {
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
                finished.write(TurnFinished {
                    agent: owner.0,
                    result: Ok(result),
                    started_at: clocks
                        .get(entity)
                        .map_or_else(Timestamp::now, |clock| clock.started_at),
                    finished_at: Timestamp::now(),
                });
                closed.push(entity);
            }
            None => {
                let Some(failure) = failures.get(entity) else {
                    continue;
                };
                finished.write(TurnFinished {
                    agent: owner.0,
                    result: Err(TurnFailure {
                        cause: failure.cause.clone(),
                    }),
                    started_at: clocks
                        .get(entity)
                        .map_or_else(Timestamp::now, |clock| clock.started_at),
                    finished_at: Timestamp::now(),
                });
                closed.push(entity);
            }
            _ => {}
        }
    }
    for turn in closed {
        for (call_entity, entry) in calls.iter() {
            if entry.turn == turn {
                commands.entity(call_entity).despawn();
            }
        }
        commands.entity(turn).despawn();
    }
}

/// 系统错误类型：只有不变量破坏才会走到这里。
pub(crate) type BoxError = anyhow::Error;

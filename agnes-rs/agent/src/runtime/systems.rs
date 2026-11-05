//! 消息管线：一组系统按注册顺序解释 [`agnes_core::agent`] 的阶段转移。
//!
//! 每个 tick 的推进与旧驱动的检查点一一对应；事件的三步落地（日志、
//! 会话、上报）在 [`record`] 里完成 —— 会话落盘经 [`RunEvent`] 消息由
//! `agnes-plugin-session` 异步完成，耐久门禁见 [`issue_model_call`]。
//!
//! 错误分级：系统返回 `Err` 仅用于不变量被破坏（缺资源、任务派发失败）——
//! 它会短路整个调度；运行期可恢复错误（模型失败、会话失败）一律转为
//! 失败收场事件与 [`TurnFinished`]，返回 `Ok`。

use std::sync::Arc;
use std::time::Instant;

use agnes_core::agent::{Advanced, Agent, Replied};
use agnes_core::content::Content;
use agnes_core::llm::StopKind;
use agnes_core::message::{Message, ToolCall, ToolOutput};
use agnes_core::tool::ToolName;
use agnes_core::turn::{AgentConfig, Outcome, StepNumber, Turn, TurnResult};
use agnes_ecs::{CancelToken, Commands, Entity, MessageReader, MessageWriter, Query, Res, ResMut};
use agnes_io::http::TransportError;
use agnes_io::llm::{LlmCall, LlmCancel, LlmError, LlmProvider, LlmReply};
use agnes_io::tool::ToolRegistry;
use agnes_plugin_async::AsyncExecutor;
use agnes_record::{ApprovalOutcome, Durability, Event as RecordEvent, Log, RunEvent, StepOutcome};
use jiff::Timestamp;

use super::RequestBuilderResource;
use super::messages::{
    ApprovalDecided, InterruptTurn, ToolFinished, TurnFailure, TurnFinished, TurnRequested,
    UserMessage,
};
use super::state::{
    CallPhase, PendingCalls, PendingTurns, PendingUserMessages, Staged, ToolCallEntry, TurnClock,
    TurnFailed, TurnPhase,
};
use crate::approval::{ApprovalChannel, ApprovalRequest};
use crate::permission::PermissionRules;

const POLICY_DENIER: &str = "permission";

/// 记录副手：日志与上报流总是结伴出现 —— 一条事件先入日志，再镜像到
/// 上报流。
type Recorder<'w> = (ResMut<'w, Log>, MessageWriter<'w, RunEvent>);

/// 一条事件的两步落地：先入日志，再发 [`RunEvent`]（会话落盘与渲染各自
/// 消费）。
fn record(recorder: &mut Recorder, event: RecordEvent) {
    let at = Timestamp::now();
    let event = recorder.0.record(event).clone();
    recorder.1.write(RunEvent::Recorded { at, event });
}

/// 自 `before` 起新增的会话消息逐条入账。
fn record_messages(recorder: &mut Recorder, turn: &Turn, before: usize) {
    for message in new_messages(turn, before) {
        record(recorder, RecordEvent::Message(message));
    }
}

/// step 与 turn 一并收场：先 `step_end`，再 `turn_end`。
fn settle_step(
    recorder: &mut Recorder,
    step: StepNumber,
    step_outcome: StepOutcome,
    turn_outcome: Outcome,
) {
    record(
        recorder,
        RecordEvent::StepEnd {
            step,
            outcome: step_outcome,
        },
    );
    record(
        recorder,
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
fn deny_entry(run_events: &mut MessageWriter<RunEvent>, entry: &mut ToolCallEntry, reason: String) {
    run_events.write(RunEvent::ToolGated {
        at: Timestamp::now(),
        call: entry.call.clone(),
        denied: agnes_record::Denied {
            by: POLICY_DENIER.into(),
            reason: reason.clone(),
        },
    });
    entry.phase = CallPhase::Denied(ToolOutput::error(reason));
}

/// 自 `before` 起新增的会话消息。
fn new_messages(turn: &Turn, before: usize) -> Vec<Message> {
    turn.messages()[before..].to_vec()
}

/// turn 开局：有活跃 turn 就排队，否则从日志重建 turn、记开局事件并启动。
///
/// 同一 tick 只 spawn 一个 turn（spawn 是延迟应用的，本 tick 内看不到），
/// 其余请求进 [`PendingTurns`]，收场后依次开跑。
pub fn begin_turn(
    mut commands: Commands,
    mut requests: MessageReader<TurnRequested>,
    turns: Query<&TurnPhase>,
    mut pending: ResMut<PendingTurns>,
    mut recorder: Recorder,
) {
    let mut spawned = false;
    for request in requests.read() {
        if spawned || !turns.is_empty() {
            pending.0.push(Arc::clone(&request));
            continue;
        }
        spawn_turn(&mut commands, &mut recorder, &request.config);
        spawned = true;
    }
    // 缓冲的请求：没有活跃 turn 时开跑一个，下一个等收场后的 tick。
    if !spawned && turns.is_empty() && !pending.is_empty() {
        let request = pending.0.remove(0);
        spawn_turn(&mut commands, &mut recorder, &request.config);
    }
}

/// spawn 一个 turn 实体：从日志重建机器、记开局事件（header、goal 消息、
/// turn 边界）并启动到待调用阶段。
fn spawn_turn(commands: &mut Commands, recorder: &mut Recorder, config: &AgentConfig) {
    let turn = Turn::with_messages(
        config.header.clone(),
        config.budget,
        recorder.0.messages().cloned().collect(),
    );
    let started_at = Timestamp::now();
    recorder.1.write(RunEvent::Started {
        at: started_at,
        goal: config.goal.clone(),
        tools: config
            .header
            .environment()
            .tools()
            .map(|spec| spec.name().clone())
            .collect(),
    });
    record(recorder, RecordEvent::Header(turn.header().clone()));
    let staged = match config.goal.clone() {
        Some(goal) => {
            record(
                recorder,
                RecordEvent::Message(Message::User { content: goal }),
            );
            Staged::AwaitingCall(
                Agent::with_goal(turn, config.goal.clone().expect("goal just read")).start(),
            )
        }
        None => Staged::AwaitingCall(Agent::new(turn).start()),
    };
    record(recorder, RecordEvent::TurnStart);
    commands.spawn_bundle((
        TurnPhase::new(staged),
        TurnClock {
            started_at,
            cancellation: CancelToken::new(),
        },
    ));
}

/// 注入排空：用户消息只在待调用阶段并入会话，其余场合在
/// [`PendingUserMessages`] 里等下一个边界。与旧驱动一致：先入账，消息
/// 不少。
pub fn inject_pending(
    mut messages: MessageReader<UserMessage>,
    mut pending_input: ResMut<PendingUserMessages>,
    mut phases: Query<&mut TurnPhase>,
    mut recorder: Recorder,
) {
    let mut queue: Vec<Content> = std::mem::take(&mut pending_input.0);
    queue.extend(messages.read().iter().map(|m| m.content.clone()));
    if queue.is_empty() {
        return;
    }
    let mut undelivered = queue.clone();
    for mut phase in phases.iter_mut() {
        let Some(Staged::AwaitingCall(agent)) = phase.0.as_mut() else {
            continue;
        };
        let before = agent.turn().messages().len();
        for content in &queue {
            agent.inject(Message::User {
                content: content.clone(),
            });
        }
        undelivered.clear();
        let injected = new_messages(agent.turn(), before);
        if injected.is_empty() {
            continue;
        }
        record(
            &mut recorder,
            RecordEvent::MessagesInjected {
                count: injected.len(),
            },
        );
        for message in injected {
            record(&mut recorder, RecordEvent::Message(message));
        }
    }
    pending_input.0 = undelivered;
}

/// 会话失败是 fail-closed：让活跃 turn 以失败收场，不再发起新调用。
/// 阶段丢弃（session repairs 兜底），插入 [`TurnFailed`] 标记，由
/// [`close_turn`] 发出失败结果并清理。
pub fn fail_on_session_failure(
    mut commands: Commands,
    durability: Option<Res<Durability>>,
    mut phases: Query<(Entity, &mut TurnPhase)>,
    mut run_events: MessageWriter<RunEvent>,
) {
    let Some(cause) = durability.as_ref().and_then(|d| d.failed.as_ref()) else {
        return;
    };
    for (entity, mut phase) in phases.iter_mut() {
        if phase.0.is_none() {
            continue;
        }
        run_events.write(RunEvent::Failed {
            at: Timestamp::now(),
            cause: cause.clone(),
        });
        phase.0 = None;
        commands.entity(entity).insert(TurnFailed {
            cause: cause.clone(),
        });
    }
}

/// 发出一次模型调用（`LlmCall` 消息），回复经 [`LlmReply`] 回注。
///
/// 耐久门禁是旧驱动的提交边界：请求发出前，已记录的日志必须已经落盘。
pub fn issue_model_call(
    provider: Option<Res<LlmProvider>>,
    requests: Res<RequestBuilderResource>,
    durability: Option<Res<Durability>>,
    mut calls: ResMut<PendingCalls>,
    mut phases: Query<(Entity, &mut TurnPhase)>,
    mut recorder: Recorder,
    mut outgoing: MessageWriter<LlmCall>,
) -> Result<(), BoxError> {
    if provider.is_none() {
        return Err(BoxError::from(
            "no LLM provider plugin is installed; add a plugin that installs `LlmProvider`",
        ));
    }
    if durability.as_ref().is_some_and(|d| !d.is_durable()) {
        // 本 tick 的记录还没落盘；等确认后再发请求。
        return Ok(());
    }

    for (entity, mut phase) in phases.iter_mut() {
        let staged = phase.0.take();
        let next = match staged {
            Some(Staged::AwaitingCall(agent)) => {
                let request = requests.build(agent.turn());
                let model = request.model.clone();
                let agent = agent.call();
                let step = agent.step();
                record(&mut recorder, RecordEvent::StepStart { step });
                record(&mut recorder, RecordEvent::MessageStart { step });
                let id = calls.issue(entity, model);
                outgoing.write(LlmCall { id, request });
                Some(Staged::AwaitingModel(agent))
            }
            other => other,
        };
        phase.0 = next;
    }
    Ok(())
}

/// 处理模型回复：正常响应推进机器（或落成工具调用实体），取消按打断
/// 收场，失败让 turn 失败收场（应用继续跑）。迟到的回复（turn 已收场或
/// 已被打断）直接丢弃。
pub fn process_llm_reply(
    mut commands: Commands,
    mut replies: MessageReader<LlmReply>,
    mut calls: ResMut<PendingCalls>,
    mut phases: Query<(Entity, &mut TurnPhase)>,
    mut recorder: Recorder,
) {
    for reply in replies.read() {
        let Some(pending) = calls.take(reply.id) else {
            continue;
        };
        let Some((entity, mut phase)) = phases.iter_mut().find(|(turn, _)| *turn == pending.turn)
        else {
            continue;
        };
        let staged = phase.0.take();
        let next = match (staged, &reply.result) {
            (Some(Staged::AwaitingModel(agent)), Ok(incoming)) => {
                let stop = StopKind::from(&incoming.stop_reason);
                let usage = incoming.usage;
                let step_outcome = StepOutcome::from(stop);
                let step = agent.step();
                let before = agent.turn().messages().len();
                match agent.respond(incoming.clone()) {
                    Replied::Tools(agent) => {
                        record_messages(&mut recorder, agent.turn(), before);
                        record(
                            &mut recorder,
                            RecordEvent::MessageEnd {
                                step,
                                model: pending.model.clone(),
                                stop,
                                usage,
                            },
                        );
                        for (index, call) in agent.calls().enumerate() {
                            commands.spawn_bundle((ToolCallEntry {
                                turn: entity,
                                step,
                                index,
                                call: call.clone(),
                                phase: CallPhase::Pending,
                            },));
                        }
                        Some(Staged::AwaitingTools(agent))
                    }
                    Replied::Closed(agent) => {
                        record_messages(&mut recorder, agent.turn(), before);
                        record(
                            &mut recorder,
                            RecordEvent::MessageEnd {
                                step,
                                model: pending.model.clone(),
                                stop,
                                usage,
                            },
                        );
                        settle_step(&mut recorder, step, step_outcome, agent.outcome());
                        Some(Staged::Closed(agent))
                    }
                }
            }
            (
                Some(Staged::AwaitingModel(agent)),
                Err(LlmError::Transport(TransportError::Cancelled)),
            ) => {
                let step = agent.step();
                let closed = agent.interrupted();
                settle_step(
                    &mut recorder,
                    step,
                    StepOutcome::Interrupted,
                    Outcome::Interrupted,
                );
                Some(Staged::Closed(closed))
            }
            (Some(Staged::AwaitingModel(_)), Err(error)) => {
                let cause = error.to_string();
                recorder.1.write(RunEvent::Failed {
                    at: Timestamp::now(),
                    cause: cause.clone(),
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
    mut entries: Query<&mut ToolCallEntry>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for mut entry in entries.iter_mut() {
        if !matches!(entry.phase, CallPhase::Pending) {
            continue;
        }
        if let Some(reason) = deny_reason(rules.as_deref(), &entry.call.name) {
            deny_entry(&mut run_events, &mut entry, reason);
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
            run_events.write(RunEvent::ApprovalResolved {
                at: Timestamp::now(),
                call_id: entry.call.id.clone(),
                outcome: ApprovalOutcome::Unavailable,
            });
            entry.phase = CallPhase::Denied(ToolOutput::error(
                "tool requires an approval, but no approval channel is wired",
            ));
            continue;
        };
        // 先发问询事件再递送：审计里不会出现“没有问询的落定”。
        run_events.write(RunEvent::ApprovalRequested {
            at: Timestamp::now(),
            call: entry.call.clone(),
        });
        let request = ApprovalRequest {
            call_id: entry.call.id.clone(),
            tool_name: entry.call.name.clone(),
            arguments: entry.call.arguments.clone(),
        };
        if let Err(err) = channel.send(request) {
            run_events.write(RunEvent::ApprovalResolved {
                at: Timestamp::now(),
                call_id: entry.call.id.clone(),
                outcome: ApprovalOutcome::Unavailable,
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
    mut entries: Query<&mut ToolCallEntry>,
    mut run_events: MessageWriter<RunEvent>,
) {
    for mut entry in entries.iter_mut() {
        let CallPhase::Asked { deadline } = &entry.phase else {
            continue;
        };
        let expired = deadline
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(false);
        if expired {
            run_events.write(RunEvent::ApprovalResolved {
                at: Timestamp::now(),
                call_id: entry.call.id.clone(),
                outcome: ApprovalOutcome::Unavailable,
            });
            entry.phase = CallPhase::Denied(ToolOutput::error("the approval timed out"));
        }
    }
    for decision in decided.read() {
        let Some(mut entry) = entries.iter_mut().find(|entry| {
            entry.call.id == decision.call_id && matches!(entry.phase, CallPhase::Asked { .. })
        }) else {
            continue;
        };
        if !decision.approved {
            run_events.write(RunEvent::ApprovalResolved {
                at: Timestamp::now(),
                call_id: entry.call.id.clone(),
                outcome: ApprovalOutcome::Rejected,
            });
            entry.phase = CallPhase::Denied(ToolOutput::error("the approval was refused"));
            continue;
        }
        run_events.write(RunEvent::ApprovalResolved {
            at: Timestamp::now(),
            call_id: entry.call.id.clone(),
            outcome: ApprovalOutcome::AllowedOnce,
        });
        if let Some(reason) = deny_reason(rules.as_deref(), &entry.call.name) {
            deny_entry(&mut run_events, &mut entry, reason);
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
    mut entries: Query<(Entity, &mut ToolCallEntry)>,
    mut recorder: Recorder,
) -> Result<(), BoxError> {
    for (entity, mut entry) in entries.iter_mut() {
        if !matches!(entry.phase, CallPhase::Allowed) {
            continue;
        }
        record(
            &mut recorder,
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
    mut phases: Query<&mut TurnPhase>,
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

        let Some(mut phase) = phases.get_mut(turn) else {
            // turn 已收场（被打断）：迟到结果直接丢弃。
            continue;
        };
        let staged = phase.0.take();
        let next = match staged {
            Some(Staged::AwaitingTools(agent)) => {
                let before = agent.turn().messages().len();
                let advanced = agent.finish(call_id.clone(), output);
                let turn = match &advanced {
                    Advanced::Ongoing(agent) => agent.turn(),
                    Advanced::Next(agent) => agent.turn(),
                    Advanced::Closed(agent) => agent.turn(),
                };
                record(
                    &mut recorder,
                    RecordEvent::ToolExecutionEnd {
                        step,
                        call_id: call_id.clone(),
                    },
                );
                record_messages(&mut recorder, turn, before);
                match advanced {
                    Advanced::Ongoing(agent) => Some(Staged::AwaitingTools(agent)),
                    Advanced::Next(agent) => {
                        record(
                            &mut recorder,
                            RecordEvent::StepEnd {
                                step,
                                outcome: StepOutcome::ToolUse,
                            },
                        );
                        Some(Staged::AwaitingCall(agent))
                    }
                    Advanced::Closed(agent) => {
                        settle_step(&mut recorder, step, StepOutcome::ToolUse, agent.outcome());
                        Some(Staged::Closed(agent))
                    }
                }
            }
            other => other,
        };
        phase.0 = next;
    }
}

/// 打断收场：按阶段走 [`Agent::interrupted`] 的对应路径，取消令牌让
/// in-flight 任务停下；迟到的回注消息会被忽略（turn 已消失）。
pub fn handle_interrupt(
    mut interrupts: MessageReader<InterruptTurn>,
    mut calls: ResMut<PendingCalls>,
    mut phases: Query<(Entity, &mut TurnPhase, &TurnClock)>,
    mut entries: Query<&mut ToolCallEntry>,
    mut recorder: Recorder,
    mut cancels: MessageWriter<LlmCancel>,
) {
    if interrupts.read().is_empty() {
        return;
    }
    for (entity, mut phase, clock) in phases.iter_mut() {
        clock.cancellation.cancel();
        for id in calls.take_for(entity) {
            cancels.write(LlmCancel { id });
        }
        let staged = phase.0.take();
        let next = match staged {
            Some(Staged::AwaitingCall(agent)) => {
                let closed = agent.interrupted();
                record(
                    &mut recorder,
                    RecordEvent::TurnEnd {
                        outcome: Outcome::Interrupted,
                    },
                );
                Some(Staged::Closed(closed))
            }
            Some(Staged::AwaitingModel(agent)) => {
                let step = agent.step();
                let closed = agent.interrupted();
                settle_step(
                    &mut recorder,
                    step,
                    StepOutcome::Interrupted,
                    Outcome::Interrupted,
                );
                Some(Staged::Closed(closed))
            }
            Some(Staged::AwaitingTools(agent)) => {
                let step = agent.step();
                let before = agent.turn().messages().len();
                let pending: Vec<ToolCall> = agent.unfinished().cloned().collect();
                let closed = agent.interrupted();
                let synthetic = new_messages(closed.turn(), before);
                for (call, message) in pending.iter().zip(synthetic) {
                    record(
                        &mut recorder,
                        RecordEvent::ToolExecutionEnd {
                            step,
                            call_id: call.id.clone(),
                        },
                    );
                    record(&mut recorder, RecordEvent::Message(message));
                }
                settle_step(
                    &mut recorder,
                    step,
                    StepOutcome::Interrupted,
                    Outcome::Interrupted,
                );
                Some(Staged::Closed(closed))
            }
            other => other,
        };
        phase.0 = next;
    }
    // 挂起的审批问询按取消落定（与旧驱动的打断路径一致）。
    for mut entry in entries.iter_mut() {
        if matches!(entry.phase, CallPhase::Asked { .. }) {
            recorder.1.write(RunEvent::ApprovalResolved {
                at: Timestamp::now(),
                call_id: entry.call.id.clone(),
                outcome: ApprovalOutcome::Cancelled,
            });
            entry.phase = CallPhase::Settled;
        }
    }
}

/// 收场清理：正常收场的 turn 发 [`TurnFinished`]，失败收场的发
/// [`TurnFinished`]（带原因）；随后 despawn turn 与它的工具调用实体。
pub fn close_turn(
    mut commands: Commands,
    mut phases: Query<(Entity, &mut TurnPhase)>,
    failures: Query<&TurnFailed>,
    clocks: Query<&TurnClock>,
    calls: Query<(Entity, &ToolCallEntry)>,
    mut run_events: MessageWriter<RunEvent>,
    mut finished: MessageWriter<TurnFinished>,
) {
    let mut closed = Vec::new();
    for (entity, mut phase) in phases.iter_mut() {
        match phase.0.as_ref() {
            Some(Staged::Closed(_)) => {
                let Some(Staged::Closed(agent)) = phase.0.take() else {
                    continue;
                };
                let result: TurnResult = agent.into_result();
                run_events.write(RunEvent::Finished {
                    at: Timestamp::now(),
                    outcome: result.outcome,
                });
                finished.write(TurnFinished {
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

/// 系统错误的盒子类型：只有不变量破坏才会走到这里。
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

//! 会话系统的三件事：转发、确认、轮转。状态全部挂在各 agent 的
//! 会话实体上（宿主经 `session_state` 挂载），系统按实体迭代与按
//! 消息里的 `agent` 字段寻址；多 agent 时每个会话的队列与耐久进度
//! 天然互相隔离。

use std::path::PathBuf;
use std::sync::Arc;

use agnes_ecs::{Entity, MessageReader, Query, Res, RuntimeSender};
use agnes_plugin_async::AsyncExecutor;
use agnes_record::{
    Durability, HostEvent, RotateSession, RunEvent, RunEventKind, SessionStore,
    SessionStoreResource,
};

use crate::{CHANNEL_CAPACITY, SessionCommand, SessionPump};

/// The writer acknowledged everything up to this sequence as durable.
#[derive(Clone, Debug)]
pub struct SessionFlushed {
    /// The agent whose durability advanced.
    pub agent: Entity,
    pub seq: u64,
}

/// The writer gave up: the session is unwritable from now on (fail-closed).
#[derive(Clone, Debug)]
pub struct SessionWriteFailed {
    /// The agent whose session failed.
    pub agent: Entity,
    pub cause: String,
}

/// The store opened the rotated file.
#[derive(Clone, Debug)]
pub struct SessionRotated {
    /// The agent whose store rotated.
    pub agent: Entity,
    pub path: PathBuf,
}

/// Forwards each session's own [`RunEvent`]s to its writer task and starts
/// the task lazily on the first run with a store installed.
///
/// Never awaits: the system only enqueues commands onto a bounded channel.
/// Run events and rotation requests are drained on every tick even when no
/// store is installed, so the message bus never piles up behind this reader.
/// [`RunEvent`]s address their agent, so every session only ever sees its
/// own stream — with several agents in one world the ledgers stay separate.
/// 会话的标识是本 crate 自己的 [`SessionPump`] 组件（宿主经
/// `session_state` 挂载），不依赖任何 agent 侧的标记。
pub fn session_pump(
    executor: Res<AsyncExecutor>,
    mut sessions: Query<(
        Entity,
        &mut SessionPump,
        &mut Durability,
        Option<&SessionStoreResource>,
    )>,
    mut run_events: MessageReader<RunEvent>,
    mut rotations: MessageReader<RotateSession>,
) {
    let events = run_events.read();
    let rotations = rotations.read();
    for (agent, mut pump, mut durability, store) in sessions.iter_mut() {
        let own_events: Vec<_> = events
            .iter()
            .filter(|event| event.agent == agent)
            .cloned()
            .collect();
        let own_rotations: Vec<_> = rotations
            .iter()
            .filter(|rotation| rotation.agent == agent)
            .cloned()
            .collect();
        pump_session(
            agent,
            &executor,
            &mut pump,
            &mut durability,
            store.as_deref(),
            &own_events,
            &own_rotations,
        );
    }
}

/// Drives one session's pump: forwards this tick's events and rotations to
/// its writer, starting the writer lazily when a store is installed.
fn pump_session(
    agent: Entity,
    executor: &AsyncExecutor,
    pump: &mut SessionPump,
    durability: &mut Durability,
    store: Option<&SessionStoreResource>,
    events: &[Arc<RunEvent>],
    rotations: &[Arc<RotateSession>],
) {
    if durability.failed.is_some() {
        return;
    }
    let Some(store) = store else {
        // 内存模式：没有落盘可等，序号随日志同步前移，报告门禁恒开。
        for run_event in events {
            if translate(&mut durability.sent_seq, run_event).is_some() {
                durability.flushed_seq = durability.sent_seq;
            }
        }
        return;
    };
    if pump.sender.is_none() {
        let (sender, receiver) = async_channel::bounded(CHANNEL_CAPACITY);
        let writer = writer_task(
            receiver,
            executor.sender(),
            Arc::clone(store.store()),
            agent,
        );
        if let Err(error) = executor.spawn(writer) {
            fail(durability, error);
            return;
        }
        pump.sender = Some(sender);
    }
    let sender = pump
        .sender
        .as_ref()
        .expect("the writer task is started above");

    for run_event in events {
        let Some(command) = translate(&mut durability.sent_seq, run_event) else {
            continue;
        };
        if let Err(error) = sender.try_send(command) {
            fail(durability, error);
            return;
        }
    }
    for _ in rotations {
        let seq = match durability.sent_seq.checked_add(1) {
            Some(seq) => seq,
            None => {
                fail(durability, "session sequence number exhausted");
                return;
            }
        };
        if let Err(error) = sender.try_send(SessionCommand::Rotate { seq }) {
            fail(durability, error);
            return;
        }
        durability.sent_seq = seq;
    }
}

/// Maps one [`RunEvent`] onto a writer command; observation-only events
/// produce nothing and do not consume a sequence number.
fn translate(next_seq: &mut u64, run_event: &RunEvent) -> Option<SessionCommand> {
    let seq = next_seq.checked_add(1)?;
    let command = match &run_event.kind {
        RunEventKind::Recorded { at, event } => SessionCommand::Append {
            seq,
            at: *at,
            event: event.clone(),
        },
        RunEventKind::ApprovalRequested { at, call } => SessionCommand::AppendHost {
            seq,
            at: *at,
            event: HostEvent::ApprovalRequested {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
        },
        RunEventKind::ApprovalResolved {
            at,
            call_id,
            outcome,
        } => SessionCommand::AppendHost {
            seq,
            at: *at,
            event: HostEvent::ApprovalResolved {
                call_id: call_id.clone(),
                outcome: *outcome,
            },
        },
        RunEventKind::Aborted { at, by, reason } => SessionCommand::AppendHost {
            seq,
            at: *at,
            event: HostEvent::Aborted {
                by: by.clone(),
                reason: reason.clone(),
            },
        },
        RunEventKind::Failed { at, cause } => SessionCommand::AppendHost {
            seq,
            at: *at,
            event: HostEvent::Failed {
                cause: cause.clone(),
            },
        },
        _ => return None,
    };
    *next_seq = seq;
    Some(command)
}

/// Marks the session unwritable; the agent runtime closes the run fail-closed.
fn fail(durability: &mut Durability, cause: impl std::fmt::Display) {
    if durability.failed.is_none() {
        log::error!("session writer failed; failing closed: {cause}");
        durability.failed = Some(cause.to_string());
    }
}

/// Applies writer acknowledgements and failures to each session's
/// [`Durability`], routed by the agent the writer belongs to.
pub fn session_ack(
    mut sessions: Query<&mut Durability>,
    mut flushed: MessageReader<SessionFlushed>,
    mut failures: MessageReader<SessionWriteFailed>,
) {
    for message in flushed.read() {
        let Some(mut durability) = sessions.get_mut(message.agent) else {
            continue;
        };
        durability.flushed_seq = durability.flushed_seq.max(message.seq);
    }
    for message in failures.read() {
        let Some(mut durability) = sessions.get_mut(message.agent) else {
            continue;
        };
        fail(&mut durability, &message.cause);
    }
}

/// Resets the targeted session's durability baseline when the host rotates it;
/// a persistence store opens the next storage and reports it as
/// [`SessionRotated`]. The in-memory log is owned by the agent runtime and is
/// reset there by `session_rotate_log`. Other agents' sessions are untouched.
pub fn session_rotate_durability(
    mut sessions: Query<&mut Durability>,
    mut requests: MessageReader<RotateSession>,
) {
    for request in requests.read() {
        if let Some(mut durability) = sessions.get_mut(request.agent) {
            durability.sent_seq = 0;
            durability.flushed_seq = 0;
        }
    }
}

/// The ordered writer: appends commands in arrival order through the store,
/// flushes after each batch, and acknowledges progress. Exits on the first
/// write failure (fail-closed) or when the command channel closes (the app is
/// gone).
async fn writer_task(
    receiver: async_channel::Receiver<SessionCommand>,
    sender: RuntimeSender,
    store: Arc<dyn SessionStore>,
    agent: Entity,
) {
    loop {
        let command = match receiver.recv().await {
            Ok(command) => command,
            Err(_) => return,
        };
        let mut acked = match apply(&store, &sender, command, agent).await {
            Ok(seq) => seq,
            Err(cause) => {
                report(&sender, cause, agent);
                return;
            }
        };
        while let Ok(command) = receiver.try_recv() {
            acked = match apply(&store, &sender, command, agent).await {
                Ok(seq) => seq,
                Err(cause) => {
                    report(&sender, cause, agent);
                    return;
                }
            };
        }
        if let Err(error) = store.flush().await {
            report(&sender, error.to_string(), agent);
            return;
        }
        if sender
            .send_message(SessionFlushed { agent, seq: acked })
            .is_err()
        {
            return;
        }
    }
}

fn report(sender: &RuntimeSender, cause: String, agent: Entity) {
    log::error!("session writer failed; failing closed: {cause}");
    let _ = sender.send_message(SessionWriteFailed { agent, cause });
}

/// Applies one command to the store, returning the command's sequence number.
async fn apply(
    store: &Arc<dyn SessionStore>,
    sender: &RuntimeSender,
    command: SessionCommand,
    agent: Entity,
) -> Result<u64, String> {
    match command {
        SessionCommand::Append { seq, at, event } => store
            .append(at, &event)
            .await
            .map(|()| seq)
            .map_err(|error| error.to_string()),
        SessionCommand::AppendHost { seq, at, event } => store
            .append_host(at, &event)
            .await
            .map(|()| seq)
            .map_err(|error| error.to_string()),
        SessionCommand::Rotate { seq } => {
            let path = store.rotate().await.map_err(|error| error.to_string())?;
            let _ = sender.send_message(SessionRotated { agent, path });
            Ok(seq)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_ecs::{App, ResMut, Update};
    use agnes_record::{Event, SessionError};
    use async_trait::async_trait;
    use jiff::Timestamp;
    use std::sync::Mutex;

    /// Records events in order; rotation yields a fixed path.
    #[derive(Default)]
    struct MemoryStore {
        events: Mutex<Vec<Event>>,
        flushes: Mutex<usize>,
    }

    #[async_trait]
    impl SessionStore for MemoryStore {
        async fn append(&self, _at: Timestamp, event: &Event) -> Result<(), SessionError> {
            self.events.lock().expect("lock").push(event.clone());
            Ok(())
        }

        async fn append_host(
            &self,
            _at: Timestamp,
            _event: &HostEvent,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn flush(&self) -> Result<(), SessionError> {
            *self.flushes.lock().expect("lock") += 1;
            Ok(())
        }

        async fn rotate(&self) -> Result<PathBuf, SessionError> {
            Ok(PathBuf::from("rotated.jsonl"))
        }
    }

    #[tokio::test]
    async fn the_writer_appends_commands_in_order_and_flushes_the_batch() {
        let memory = Arc::new(MemoryStore::default());
        let (sender_tx, receiver) = async_channel::bounded(8);
        let app = App::new();

        sender_tx
            .send(SessionCommand::Append {
                seq: 1,
                at: Timestamp::now(),
                event: Event::TurnStart,
            })
            .await
            .expect("send");
        sender_tx
            .send(SessionCommand::Append {
                seq: 2,
                at: Timestamp::now(),
                event: Event::MessagesInjected { count: 1 },
            })
            .await
            .expect("send");
        drop(sender_tx);

        writer_task(
            receiver,
            app.runtime_sender(),
            memory.clone(),
            agnes_ecs::Entity::PLACEHOLDER,
        )
        .await;

        let events = memory.events.lock().expect("lock").clone();
        assert!(
            matches!(
                events.as_slice(),
                [Event::TurnStart, Event::MessagesInjected { count: 1 }]
            ),
            "the writer appends in arrival order"
        );
        assert_eq!(*memory.flushes.lock().expect("lock"), 1);
    }

    #[tokio::test]
    async fn the_writer_acks_every_durable_batch() {
        let memory = Arc::new(MemoryStore::default());
        let (sender_tx, receiver) = async_channel::bounded(8);
        let mut app = App::new();
        app.insert_resource(Acked::default());
        app.add_systems(Update, capture_acks);

        sender_tx
            .send(SessionCommand::Append {
                seq: 1,
                at: Timestamp::now(),
                event: Event::TurnStart,
            })
            .await
            .expect("send");
        sender_tx
            .send(SessionCommand::Append {
                seq: 2,
                at: Timestamp::now(),
                event: Event::MessagesInjected { count: 1 },
            })
            .await
            .expect("send");
        drop(sender_tx);

        writer_task(
            receiver,
            app.runtime_sender(),
            memory,
            agnes_ecs::Entity::PLACEHOLDER,
        )
        .await;
        app.pump().expect("apply the ack");

        let acked = app.world().resource::<Acked>().expect("acked is installed");
        assert_eq!(acked.0, vec![2], "the whole batch is acknowledged");
    }

    #[tokio::test]
    async fn rotation_reports_the_new_path() {
        let memory = Arc::new(MemoryStore::default());
        let (sender_tx, receiver) = async_channel::bounded(8);
        let mut app = App::new();
        app.insert_resource(Rotated::default());
        app.add_systems(Update, capture_rotated);

        sender_tx
            .send(SessionCommand::Rotate { seq: 1 })
            .await
            .expect("send");
        drop(sender_tx);
        writer_task(
            receiver,
            app.runtime_sender(),
            memory,
            agnes_ecs::Entity::PLACEHOLDER,
        )
        .await;
        app.pump().expect("apply the rotation report");

        let rotated = app
            .world()
            .resource::<Rotated>()
            .expect("rotated capture is installed");
        assert_eq!(rotated.0, vec![PathBuf::from("rotated.jsonl")]);
    }

    #[test]
    fn a_write_failure_marks_the_session_failed_and_stops_the_pump() {
        let mut app = App::new();
        app.add_plugin(agnes_plugin_async::TokioPlugin);
        let root = app.world_mut().spawn_empty();
        app.world_mut().insert(
            root,
            Durability {
                sent_seq: 0,
                flushed_seq: 0,
                failed: None,
            },
        );
        app.world_mut().insert(root, SessionPump::default());
        // ack 先于 pump：失败在本 tick 内生效，pump 当场短路。
        app.add_systems(Update, session_ack);
        app.add_systems(Update, session_pump);

        let sender = app.runtime_sender();
        sender
            .send_message(SessionWriteFailed {
                agent: root,
                cause: "disk full".into(),
            })
            .expect("runtime is alive");
        app.pump().expect("schedule runs");
        assert_eq!(
            app.world()
                .get::<Durability>(root)
                .expect("durability is installed")
                .failed
                .as_deref(),
            Some("disk full"),
        );

        // fail-closed：之后的事件不再转发，序号不再前进。
        sender
            .send_message(RunEvent {
                agent: root,
                kind: RunEventKind::Recorded {
                    at: Timestamp::now(),
                    event: Event::TurnStart,
                },
            })
            .expect("runtime is alive");
        app.pump().expect("schedule runs");
        assert_eq!(
            app.world()
                .get::<Durability>(root)
                .expect("durability is installed")
                .sent_seq,
            0,
            "nothing is forwarded after the failure",
        );
    }

    #[derive(Default)]
    struct Acked(Vec<u64>);

    fn capture_acks(mut reader: MessageReader<SessionFlushed>, mut acked: ResMut<Acked>) {
        acked
            .0
            .extend(reader.read().iter().map(|message| message.seq));
    }

    #[derive(Default)]
    struct Rotated(Vec<PathBuf>);

    fn capture_rotated(mut reader: MessageReader<SessionRotated>, mut rotated: ResMut<Rotated>) {
        rotated
            .0
            .extend(reader.read().iter().map(|message| message.path.clone()));
    }
}

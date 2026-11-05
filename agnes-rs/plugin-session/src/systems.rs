//! 会话系统的三件事：转发、确认、轮转。

use std::path::PathBuf;
use std::sync::Arc;

use agnes_ecs::{MessageReader, Res, ResMut, RuntimeSender};
use agnes_plugin_async::AsyncExecutor;
use agnes_record::{
    Durability, HostEvent, Log, RotateSession, RunEvent, SessionStore, SessionStoreResource,
};

use crate::{CHANNEL_CAPACITY, SessionCommand, SessionPump};

/// The writer acknowledged everything up to this sequence as durable.
#[derive(Clone, Debug)]
pub struct SessionFlushed {
    pub seq: u64,
}

/// The writer gave up: the session is unwritable from now on (fail-closed).
#[derive(Clone, Debug)]
pub struct SessionWriteFailed {
    pub cause: String,
}

/// The store opened the rotated file.
#[derive(Clone, Debug)]
pub struct SessionRotated {
    pub path: PathBuf,
}

/// Forwards session-relevant [`RunEvent`]s to the writer task and starts the
/// task lazily on the first run with a store installed.
///
/// Never awaits: the system only enqueues commands onto a bounded channel.
/// Run events and rotation requests are drained on every tick even when no
/// store is installed, so the message bus never piles up behind this reader.
pub fn session_pump(
    executor: Res<AsyncExecutor>,
    mut pump: ResMut<SessionPump>,
    mut durability: ResMut<Durability>,
    store: Option<Res<SessionStoreResource>>,
    mut run_events: MessageReader<RunEvent>,
    mut rotations: MessageReader<RotateSession>,
) {
    let events = run_events.read();
    let rotations = rotations.read();
    if durability.failed.is_some() {
        return;
    }
    let Some(store) = store else {
        // 内存模式：没有落盘可等，序号随日志同步前移，报告门禁恒开。
        for run_event in &events {
            if translate(&mut durability.sent_seq, run_event).is_some() {
                durability.flushed_seq = durability.sent_seq;
            }
        }
        return;
    };
    if pump.sender.is_none() {
        let (sender, receiver) = async_channel::bounded(CHANNEL_CAPACITY);
        let writer = writer_task(receiver, executor.sender(), Arc::clone(store.store()));
        if let Err(error) = executor.spawn(writer) {
            fail(&mut durability, error);
            return;
        }
        pump.sender = Some(sender);
    }
    let sender = pump
        .sender
        .as_ref()
        .expect("the writer task is started above");

    for run_event in events {
        let Some(command) = translate(&mut durability.sent_seq, &run_event) else {
            continue;
        };
        if let Err(error) = sender.try_send(command) {
            fail(&mut durability, error);
            return;
        }
    }
    for _ in rotations {
        let seq = match durability.sent_seq.checked_add(1) {
            Some(seq) => seq,
            None => {
                fail(&mut durability, "session sequence number exhausted");
                return;
            }
        };
        if let Err(error) = sender.try_send(SessionCommand::Rotate { seq }) {
            fail(&mut durability, error);
            return;
        }
        durability.sent_seq = seq;
    }
}

/// Maps one [`RunEvent`] onto a writer command; observation-only events
/// produce nothing and do not consume a sequence number.
fn translate(next_seq: &mut u64, run_event: &RunEvent) -> Option<SessionCommand> {
    let seq = next_seq.checked_add(1)?;
    let command = match run_event {
        RunEvent::Recorded { at, event } => SessionCommand::Append {
            seq,
            at: *at,
            event: event.clone(),
        },
        RunEvent::ApprovalRequested { at, call } => SessionCommand::AppendHost {
            seq,
            at: *at,
            event: HostEvent::ApprovalRequested {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
        },
        RunEvent::ApprovalResolved {
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
        RunEvent::Aborted { at, by, reason } => SessionCommand::AppendHost {
            seq,
            at: *at,
            event: HostEvent::Aborted {
                by: by.clone(),
                reason: reason.clone(),
            },
        },
        RunEvent::Failed { at, cause } => SessionCommand::AppendHost {
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

/// Applies writer acknowledgements and failures to the [`Durability`] state.
pub fn session_ack(
    mut durability: ResMut<Durability>,
    mut flushed: MessageReader<SessionFlushed>,
    mut failures: MessageReader<SessionWriteFailed>,
) {
    for message in flushed.read() {
        durability.flushed_seq = durability.flushed_seq.max(message.seq);
    }
    for message in failures.read() {
        fail(&mut durability, &message.cause);
    }
}

/// Clears the in-memory log and the durability baseline when the host rotates
/// the session; a persistence store opens the next storage and reports it as
/// [`SessionRotated`].
pub fn session_rotate(
    mut log: ResMut<Log>,
    mut durability: ResMut<Durability>,
    mut requests: MessageReader<RotateSession>,
) {
    if requests.read().is_empty() {
        return;
    }
    log.reset();
    durability.sent_seq = 0;
    durability.flushed_seq = 0;
}

/// The ordered writer: appends commands in arrival order through the store,
/// flushes after each batch, and acknowledges progress. Exits on the first
/// write failure (fail-closed) or when the command channel closes (the app is
/// gone).
async fn writer_task(
    receiver: async_channel::Receiver<SessionCommand>,
    sender: RuntimeSender,
    store: Arc<dyn SessionStore>,
) {
    loop {
        let command = match receiver.recv().await {
            Ok(command) => command,
            Err(_) => return,
        };
        let mut acked = match apply(&store, &sender, command).await {
            Ok(seq) => seq,
            Err(cause) => {
                report(&sender, cause);
                return;
            }
        };
        while let Ok(command) = receiver.try_recv() {
            acked = match apply(&store, &sender, command).await {
                Ok(seq) => seq,
                Err(cause) => {
                    report(&sender, cause);
                    return;
                }
            };
        }
        if let Err(error) = store.flush().await {
            report(&sender, error.to_string());
            return;
        }
        if sender.send_message(SessionFlushed { seq: acked }).is_err() {
            return;
        }
    }
}

fn report(sender: &RuntimeSender, cause: String) {
    log::error!("session writer failed; failing closed: {cause}");
    let _ = sender.send_message(SessionWriteFailed { cause });
}

/// Applies one command to the store, returning the command's sequence number.
async fn apply(
    store: &Arc<dyn SessionStore>,
    sender: &RuntimeSender,
    command: SessionCommand,
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
            let _ = sender.send_message(SessionRotated { path });
            Ok(seq)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_ecs::{App, Update};
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

        writer_task(receiver, app.runtime_sender(), memory.clone()).await;

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

        writer_task(receiver, app.runtime_sender(), memory).await;
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
        writer_task(receiver, app.runtime_sender(), memory).await;
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
        app.insert_resource(Durability {
            sent_seq: 0,
            flushed_seq: 0,
            failed: None,
        });
        app.insert_resource(SessionPump::default());
        // ack 先于 pump：失败在本 tick 内生效，pump 当场短路。
        app.add_systems(Update, session_ack);
        app.add_systems(Update, session_pump);

        let sender = app.runtime_sender();
        sender
            .send_message(SessionWriteFailed {
                cause: "disk full".into(),
            })
            .expect("runtime is alive");
        app.pump().expect("schedule runs");
        assert_eq!(
            app.world()
                .resource::<Durability>()
                .expect("durability is installed")
                .failed
                .as_deref(),
            Some("disk full"),
        );

        // fail-closed：之后的事件不再转发，序号不再前进。
        sender
            .send_message(RunEvent::Recorded {
                at: Timestamp::now(),
                event: Event::TurnStart,
            })
            .expect("runtime is alive");
        app.pump().expect("schedule runs");
        assert_eq!(
            app.world()
                .resource::<Durability>()
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

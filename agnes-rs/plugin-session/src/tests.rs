use super::*;
use agnes_core::config::SystemPrompt;
use agnes_core::header::{Environment, Header};
use agnes_ecs::Entity;
use agnes_plugin_async::TokioPlugin;
use agnes_record::{
    Event, HostEvent, RunEvent, RunEventKind, SessionError, SessionStore, SessionStoreResource,
};
use async_trait::async_trait;
use jiff::Timestamp;
use std::sync::{Arc, Mutex};

fn header() -> Event {
    Event::Header(Header::new(
        SystemPrompt::new("prompt"),
        Environment::default(),
    ))
}

/// Spawns a session carrying `session_state(events)` — the host's job. The log
/// goes into the agent's core state, so this plugin-only fixture drops it.
fn install(app: &mut App, events: &[Event]) -> Entity {
    let SessionState {
        pump, durability, ..
    } = session_state(events).expect("session state");
    app.world_mut().spawn_bundle((pump, durability))
}

fn session_root(app: &App) -> Entity {
    app.world()
        .entity_with::<SessionPump>()
        .expect("session root")
}

fn durability(app: &App) -> Durability {
    app.world()
        .get::<Durability>(session_root(app))
        .expect("durability component")
        .clone()
}

#[test]
fn session_state_restores_the_log_and_durability_baseline() {
    let SessionState {
        log, durability, ..
    } = session_state(&[header()]).expect("state");

    assert_eq!(log.events().len(), 1);
    assert!(log.header().is_some());
    assert_eq!(durability.sent_seq, 1);
    assert_eq!(durability.flushed_seq, 1);
    assert!(
        durability.is_durable(),
        "restored history is already durable"
    );
}

#[test]
fn a_history_without_a_header_is_refused() {
    let error = match session_state(&[Event::TurnStart]) {
        Err(error) => error,
        Ok(_) => panic!("a history without a header must be refused"),
    };
    assert!(matches!(error, SessionInitError::NoHeader));
}

#[test]
fn the_plugin_never_creates_a_session() {
    let mut app = App::new();
    app.add_plugin(SessionPlugin);

    assert!(
        app.world().entity_with::<SessionPump>().is_none(),
        "state attachment is the host's job"
    );
}

#[test]
fn a_rotation_resets_only_the_targeted_sessions_durability() {
    let mut app = App::new();
    let first = install(&mut app, &[]);
    let second = install(&mut app, &[]);
    app.add_plugin(SessionPlugin).add_plugin(TokioPlugin);
    for agent in [first, second] {
        let mut durability = app
            .world_mut()
            .get_mut::<Durability>(agent)
            .expect("durability component");
        durability.sent_seq = 3;
        durability.flushed_seq = 3;
    }

    app.world_mut().send_message(RotateSession { agent: first });
    app.pump().expect("schedule runs");

    let first = app
        .world()
        .get::<Durability>(first)
        .expect("durability component")
        .clone();
    assert_eq!(first.sent_seq, 0);
    assert_eq!(first.flushed_seq, 0, "the target's baseline is reset");
    assert!(
        first.is_durable(),
        "the rotated memory-only session has nothing to wait for"
    );

    let second = app
        .world()
        .get::<Durability>(second)
        .expect("durability component")
        .clone();
    assert_eq!(
        second.sent_seq, 3,
        "the other session's ledger is untouched"
    );
}

#[test]
fn memory_only_sessions_track_durability_in_lockstep() {
    let mut app = App::new();
    let agent = install(&mut app, &[]);
    app.add_plugin(SessionPlugin).add_plugin(TokioPlugin);
    app.world_mut().send_message(RunEvent {
        agent,
        kind: RunEventKind::Recorded {
            at: Timestamp::now(),
            event: Event::TurnStart,
        },
    });
    app.pump().expect("schedule runs");

    let durability = durability(&app);
    assert_eq!(durability.sent_seq, 1);
    assert_eq!(
        durability.flushed_seq, 1,
        "nothing to wait for without a store"
    );
    assert!(durability.is_durable());
}

#[tokio::test]
async fn an_event_only_reaches_its_own_session() {
    let first_memory = Arc::new(MemoryStore::default());
    let second_memory = Arc::new(MemoryStore::default());
    let mut app = App::new();
    let first = install(&mut app, &[]);
    let second = install(&mut app, &[]);
    let first_store: Arc<dyn SessionStore> = Arc::clone(&first_memory) as Arc<dyn SessionStore>;
    let second_store: Arc<dyn SessionStore> = Arc::clone(&second_memory) as Arc<dyn SessionStore>;
    app.world_mut()
        .insert(first, SessionStoreResource::new(first_store));
    app.world_mut()
        .insert(second, SessionStoreResource::new(second_store));
    app.add_plugin(SessionPlugin).add_plugin(TokioPlugin);

    app.runtime_sender()
        .send_message(RunEvent {
            agent: first,
            kind: RunEventKind::Recorded {
                at: Timestamp::now(),
                event: Event::TurnStart,
            },
        })
        .expect("runtime is alive");

    for _ in 0..64 {
        app.pump().expect("schedule runs");
        tokio::task::yield_now().await;
        if !first_memory.events.lock().expect("lock").is_empty() {
            break;
        }
    }

    assert!(
        matches!(
            first_memory.events.lock().expect("lock").as_slice(),
            [Event::TurnStart]
        ),
        "the event reached its own session's store"
    );
    assert!(
        second_memory.events.lock().expect("lock").is_empty(),
        "the other session's store never sees a foreign event"
    );
    assert_eq!(
        app.world()
            .get::<Durability>(second)
            .expect("durability component")
            .sent_seq,
        0,
        "the other session's ledger never advances"
    );
}

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

    async fn append_host(&self, _at: Timestamp, _event: &HostEvent) -> Result<(), SessionError> {
        Ok(())
    }

    async fn flush(&self) -> Result<(), SessionError> {
        *self.flushes.lock().expect("lock") += 1;
        Ok(())
    }

    async fn rotate(&self) -> Result<std::path::PathBuf, SessionError> {
        Ok(std::path::PathBuf::from("rotated.jsonl"))
    }
}

#[tokio::test]
async fn a_run_event_reaches_the_installed_store_and_is_acked() {
    let memory = Arc::new(MemoryStore::default());
    let store: Arc<dyn SessionStore> = Arc::clone(&memory) as Arc<dyn SessionStore>;
    let mut app = App::new();
    let agent = install(&mut app, &[]);
    app.world_mut()
        .insert(agent, SessionStoreResource::new(store));
    app.add_plugin(SessionPlugin).add_plugin(TokioPlugin);

    app.runtime_sender()
        .send_message(RunEvent {
            agent,
            kind: RunEventKind::Recorded {
                at: Timestamp::now(),
                event: Event::TurnStart,
            },
        })
        .expect("runtime is alive");

    for _ in 0..64 {
        app.pump().expect("schedule runs");
        tokio::task::yield_now().await;
        if durability(&app).is_durable() && !memory.events.lock().expect("lock").is_empty() {
            break;
        }
    }

    assert!(
        matches!(
            memory.events.lock().expect("lock").as_slice(),
            [Event::TurnStart]
        ),
        "the run event reached the store"
    );
    assert!(
        durability(&app).is_durable(),
        "the writer acknowledged the batch"
    );
    assert_eq!(*memory.flushes.lock().expect("lock"), 1);
}

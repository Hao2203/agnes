use super::*;
use agnes_core::config::SystemPrompt;
use agnes_core::header::{Environment, Header};
use agnes_ecs::Entity;
use agnes_plugin_async::TokioPlugin;
use agnes_record::{
    AgentRoot, Event, HostEvent, RunEvent, SessionError, SessionStore, SessionStoreResource,
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

/// The agent root the plugin attached the session state to.
fn root(app: &App) -> Entity {
    app.world().entity_with::<AgentRoot>().expect("agent root")
}

fn log(app: &App) -> Log {
    app.world()
        .get::<Log>(root(app))
        .expect("log component")
        .clone()
}

fn durability(app: &App) -> Durability {
    app.world()
        .get::<Durability>(root(app))
        .expect("durability component")
        .clone()
}

#[test]
fn seeds_the_log_and_installs_the_session_state() {
    let mut app = App::new();
    app.add_plugin(SessionPlugin::resumed(vec![header()]).expect("plugin"));

    let log = log(&app);
    assert_eq!(log.events().len(), 1);
    assert!(log.header().is_some());
    app.world()
        .get::<Durability>(root(&app))
        .expect("durability component");
    app.world()
        .get::<SessionPump>(root(&app))
        .expect("pump component");
}

#[test]
fn a_history_without_a_header_is_refused() {
    let error = match SessionPlugin::resumed(vec![Event::TurnStart]) {
        Err(error) => error,
        Ok(_) => panic!("a history without a header must be refused"),
    };
    assert!(matches!(error, SessionInitError::NoHeader));
}

#[test]
fn a_rotation_resets_the_log_without_a_store() {
    let mut app = App::new();
    app.add_plugin(SessionPlugin::new()).add_plugin(TokioPlugin);
    let agent = root(&app);
    app.world_mut()
        .get_mut::<Log>(agent)
        .expect("log component")
        .record(Event::TurnStart);
    assert_eq!(log(&app).events().len(), 1);

    app.world_mut().send_message(RotateSession);
    app.pump().expect("schedule runs");

    assert!(
        log(&app).events().is_empty(),
        "the in-memory context is cleared"
    );
    assert!(
        durability(&app).is_durable(),
        "memory-only sessions are always durable"
    );
}

#[test]
fn memory_only_sessions_track_durability_in_lockstep() {
    let mut app = App::new();
    app.add_plugin(SessionPlugin::new()).add_plugin(TokioPlugin);
    app.world_mut().send_message(RunEvent::Recorded {
        at: Timestamp::now(),
        event: Event::TurnStart,
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
    app.add_plugin(SessionPlugin::new());
    let root = app.world().entity_with::<AgentRoot>().expect("agent root");
    app.world_mut()
        .insert(root, SessionStoreResource::new(store));
    app.add_plugin(TokioPlugin);

    app.runtime_sender()
        .send_message(RunEvent::Recorded {
            at: Timestamp::now(),
            event: Event::TurnStart,
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

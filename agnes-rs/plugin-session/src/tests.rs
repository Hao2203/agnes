use super::*;
use agnes_core::config::SystemPrompt;
use agnes_core::header::{Environment, Header};
use agnes_plugin_async::TokioPlugin;
use agnes_record::{Event, HostEvent, RunEvent, SessionError, SessionStore, SessionStoreResource};
use async_trait::async_trait;
use jiff::Timestamp;
use std::sync::{Arc, Mutex};

fn header() -> Event {
    Event::Header(Header::new(
        SystemPrompt::new("prompt"),
        Environment::default(),
    ))
}

#[test]
fn seeds_the_log_and_installs_the_session_state() {
    let mut app = App::new();
    app.add_plugin(SessionPlugin::resumed(vec![header()]).expect("plugin"));

    let log = app.world().resource::<Log>().expect("log resource");
    assert_eq!(log.events().len(), 1);
    assert!(log.header().is_some());
    app.world()
        .resource::<Durability>()
        .expect("durability resource");
    app.world()
        .resource::<SessionPump>()
        .expect("pump resource");
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
    app.world_mut()
        .resource_mut::<Log>()
        .expect("log resource")
        .record(Event::TurnStart);
    assert_eq!(
        app.world().resource::<Log>().expect("log").events().len(),
        1
    );

    app.world_mut().send_message(RotateSession);
    app.pump().expect("schedule runs");

    assert!(
        app.world()
            .resource::<Log>()
            .expect("log")
            .events()
            .is_empty(),
        "the in-memory context is cleared"
    );
    assert!(
        app.world()
            .resource::<Durability>()
            .expect("durability")
            .is_durable(),
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

    let durability = app.world().resource::<Durability>().expect("durability");
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
    app.add_plugin(SessionPlugin::new())
        .insert_resource(SessionStoreResource::new(store))
        .add_plugin(TokioPlugin);

    app.runtime_sender()
        .send_message(RunEvent::Recorded {
            at: Timestamp::now(),
            event: Event::TurnStart,
        })
        .expect("runtime is alive");

    for _ in 0..64 {
        app.pump().expect("schedule runs");
        tokio::task::yield_now().await;
        let durable = app
            .world()
            .resource::<Durability>()
            .expect("durability")
            .is_durable();
        if durable && !memory.events.lock().expect("lock").is_empty() {
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
        app.world()
            .resource::<Durability>()
            .expect("durability")
            .is_durable(),
        "the writer acknowledged the batch"
    );
    assert_eq!(*memory.flushes.lock().expect("lock"), 1);
}

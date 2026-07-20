use agnes_llm::{MockProvider, Provider};
use agnes_session::{EventSink, Session, SessionError, SessionEvent, TurnInput};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

fn test_lock() -> &'static std::sync::Mutex<()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
}

#[derive(Default)]
struct Recording(Arc<Mutex<Vec<SessionEvent>>>);
#[async_trait]
impl EventSink for Recording {
    async fn emit(&mut self, ev: SessionEvent) {
        self.0.lock().unwrap().push(ev);
    }
}

#[tokio::test]
async fn cancel_before_first_iteration_returns_cancelled_with_zero() {
    let _g = test_lock().lock().unwrap();
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));
    let mut s = Session::new(provider).unwrap();
    let mut sink = Recording::default();
    let cancel = Arc::new(Notify::new());
    // Pre-notify: the loop should see it on the very first check.
    cancel.notify_one();
    let err = s
        .run_turn_cancellable(TurnInput::NaturalLanguage("go".into()), &mut sink, cancel)
        .await
        .expect_err("expected cancelled");
    match err {
        SessionError::Cancelled { after_iterations } => assert_eq!(after_iterations, 0),
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

#[tokio::test]
async fn cancel_between_iterations_stops_after_current_iteration() {
    let _g = test_lock().lock().unwrap();
    // Provider always says observe (loop wants to continue). We fire the
    // cancel after the first ObservationEmitted arrives.
    let responses: Vec<String> = (0..10)
        .map(|i| format!("```agnes\n(pipe \"iter {i}\" observe)\n```"))
        .collect();
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(responses));
    let mut s = Session::new(provider).unwrap();

    let ev_log = Arc::new(Mutex::new(Vec::new()));
    struct Sink(Arc<Mutex<Vec<SessionEvent>>>, Arc<Notify>);
    #[async_trait]
    impl EventSink for Sink {
        async fn emit(&mut self, ev: SessionEvent) {
            let is_obs = matches!(ev, SessionEvent::ObservationEmitted { .. });
            self.0.lock().unwrap().push(ev);
            // Fire cancel immediately after the FIRST observation.
            if is_obs
                && self
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| matches!(e, SessionEvent::ObservationEmitted { .. }))
                    .count()
                    == 1
            {
                self.1.notify_one();
            }
        }
    }

    let cancel = Arc::new(Notify::new());
    let mut sink = Sink(ev_log.clone(), cancel.clone());
    let err = s
        .run_turn_cancellable(TurnInput::NaturalLanguage("go".into()), &mut sink, cancel)
        .await
        .expect_err("expected cancelled");
    match err {
        SessionError::Cancelled { after_iterations } => {
            // Exactly one iteration ran (iter 0), so after_iterations = 1.
            assert_eq!(after_iterations, 1);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    let evs = ev_log.lock().unwrap();
    let has_failed = evs
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnFailed { .. }));
    assert!(has_failed);
}

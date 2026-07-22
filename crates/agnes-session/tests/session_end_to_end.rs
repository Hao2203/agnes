//! Session integration tests: exercise the multi-iteration agent loop
//! against MockProvider. No real network.

use agnes_llm::{MockProvider, Provider};
use agnes_session::{EventSink, Session, SessionError, SessionEvent, TurnInput};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;

/// Serialize integration tests that share the process-global writes()
/// recorder in agnes-builtins.
fn test_lock() -> &'static std::sync::Mutex<()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
}

#[derive(Default)]
struct RecordingSink(Arc<Mutex<Vec<SessionEvent>>>);

impl RecordingSink {
    fn events(&self) -> Vec<SessionEvent> {
        self.0.lock().unwrap().clone()
    }
    fn shared(&self) -> Arc<Mutex<Vec<SessionEvent>>> {
        Arc::clone(&self.0)
    }
}

#[async_trait]
impl EventSink for RecordingSink {
    async fn emit(&mut self, ev: SessionEvent) {
        self.0.lock().unwrap().push(ev);
    }
}

fn provider(responses: Vec<&str>) -> Arc<dyn Provider> {
    Arc::new(MockProvider::new(
        responses.into_iter().map(String::from).collect(),
    ))
}

#[tokio::test]
async fn single_iteration_with_explicit_finish() {
    let _g = test_lock().lock().unwrap();
    let mut s = Session::new(provider(vec!["```agnes\n(pipe \"done\" finish)\n```"])).unwrap();
    let sink = RecordingSink::default();
    let sink = Arc::new(TokioMutex::new(sink));
    let v = s
        .run_turn(TurnInput::NaturalLanguage("hi".into()), sink.clone())
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("done"));
    let evs = sink.lock().await.events();
    let has_iter_0 = evs
        .iter()
        .any(|e| matches!(e, SessionEvent::IterationStart { iter: 0 }));
    let has_turn_result = evs
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnResult { .. }));
    assert!(has_iter_0);
    assert!(has_turn_result);
}

#[tokio::test]
async fn unlabeled_result_is_implicit_finish() {
    let _g = test_lock().lock().unwrap();
    // No finish or observe. Result is PlainText; Session treats as implicit finish.
    let mut s = Session::new(provider(vec!["```agnes\n\"hello\"\n```"])).unwrap();
    let sink = RecordingSink::default();
    let sink = Arc::new(TokioMutex::new(sink));
    let v = s
        .run_turn(TurnInput::NaturalLanguage("say hi".into()), sink.clone())
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("hello"));
    let evs = sink.lock().await.events();
    // Only one iteration.
    let iter_starts = evs
        .iter()
        .filter(|e| matches!(e, SessionEvent::IterationStart { .. }))
        .count();
    assert_eq!(iter_starts, 1);
}

#[tokio::test]
async fn observation_feeds_back_and_second_iteration_finishes() {
    let _g = test_lock().lock().unwrap();
    let mut s = Session::new(provider(vec![
        "```agnes\n(pipe \"first\" observe)\n```",
        "```agnes\n(pipe \"final\" finish)\n```",
    ]))
    .unwrap();
    let sink = RecordingSink::default();
    let sink = Arc::new(TokioMutex::new(sink));
    let v = s
        .run_turn(TurnInput::NaturalLanguage("go".into()), sink.clone())
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("final"));
    let evs = sink.lock().await.events();
    // Two IterationStart events: iter=0 and iter=1.
    assert!(
        evs.iter()
            .any(|e| matches!(e, SessionEvent::IterationStart { iter: 0 }))
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, SessionEvent::IterationStart { iter: 1 }))
    );
    // ObservationEmitted with is_error=false, iter=0.
    let has_obs = evs.iter().any(|e| {
        matches!(
            e,
            SessionEvent::ObservationEmitted {
                iter: 0,
                is_error: false,
                text
            } if text == "first"
        )
    });
    assert!(
        has_obs,
        "expected ObservationEmitted iter=0 text=first, got {evs:?}"
    );
}

#[tokio::test]
async fn parse_error_feeds_back_and_self_heals() {
    let _g = test_lock().lock().unwrap();
    let mut s = Session::new(provider(vec![
        "```agnes\n((this is not valid\n```",
        "```agnes\n(pipe \"recovered\" finish)\n```",
    ]))
    .unwrap();
    let sink = RecordingSink::default();
    let sink = Arc::new(TokioMutex::new(sink));
    let v = s
        .run_turn(TurnInput::NaturalLanguage("go".into()), sink.clone())
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("recovered"));
    let evs = sink.lock().await.events();
    let has_err_obs = evs.iter().any(|e| {
        matches!(
            e,
            SessionEvent::ObservationEmitted {
                iter: 0,
                is_error: true,
                ..
            }
        )
    });
    assert!(
        has_err_obs,
        "expected an error observation for iter 0, got {evs:?}"
    );
}

#[tokio::test]
async fn raw_dsl_seeds_iteration_zero_and_continues_on_observation() {
    let _g = test_lock().lock().unwrap();
    // /run (pipe "seed" observe) → iter 0 uses the raw DSL, produces
    // Observation, feeds back; planner fills iter 1.
    let mut s = Session::new(provider(vec!["```agnes\n(pipe \"planned\" finish)\n```"])).unwrap();
    let sink = RecordingSink::default();
    let sink = Arc::new(TokioMutex::new(sink));
    let v = s
        .run_turn(
            TurnInput::RawDsl("(pipe \"seed\" observe)".into()),
            sink.clone(),
        )
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("planned"));
    // Two iterations: iter 0 (raw) and iter 1 (planner-produced).
    let iter_count = sink
        .lock()
        .await
        .events()
        .iter()
        .filter(|e| matches!(e, SessionEvent::IterationStart { .. }))
        .count();
    assert_eq!(iter_count, 2);
}

#[tokio::test]
async fn raw_dsl_that_finishes_directly_stops_after_one_iteration() {
    let _g = test_lock().lock().unwrap();
    // /run (pipe "just this" finish) — should terminate in one iteration,
    // planner is never consulted (empty response queue is fine).
    let mut s = Session::new(provider(vec![])).unwrap();
    let sink = RecordingSink::default();
    let sink = Arc::new(TokioMutex::new(sink));
    let v = s
        .run_turn(
            TurnInput::RawDsl("(pipe \"just this\" finish)".into()),
            sink.clone(),
        )
        .await
        .unwrap();
    assert_eq!(v.data.as_str(), Some("just this"));
    let iter_count = sink
        .lock()
        .await
        .events()
        .iter()
        .filter(|e| matches!(e, SessionEvent::IterationStart { .. }))
        .count();
    assert_eq!(iter_count, 1);
}

#[tokio::test]
async fn max_turns_ceiling_terminates_with_turn_limit_exceeded() {
    let _g = test_lock().lock().unwrap();
    // Planner always returns observe → never terminates on its own.
    // Set max_turns=3 and expect TurnLimitExceeded.
    let responses: Vec<String> = (0..10)
        .map(|i| format!("```agnes\n(pipe \"iter {i}\" observe)\n```"))
        .collect();
    let mut s =
        Session::new_with_max_turns(Arc::new(MockProvider::new(responses.clone())), 3).unwrap();
    let sink = RecordingSink::default();
    let sink = Arc::new(TokioMutex::new(sink));
    let err = s
        .run_turn(TurnInput::NaturalLanguage("go".into()), sink.clone())
        .await
        .expect_err("must exceed limit");
    match err {
        SessionError::TurnLimitExceeded { max_turns } => assert_eq!(max_turns, 3),
        other => panic!("expected TurnLimitExceeded, got {other:?}"),
    }
    // Exactly 3 IterationStart events fired.
    let iter_count = sink
        .lock()
        .await
        .events()
        .iter()
        .filter(|e| matches!(e, SessionEvent::IterationStart { .. }))
        .count();
    assert_eq!(iter_count, 3);
    // TurnFailed was emitted before returning Err.
    let has_failed = sink
        .lock()
        .await
        .events()
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnFailed { .. }));
    assert!(has_failed);
}

#[tokio::test]
async fn write_summary_still_emitted_before_turn_result() {
    let _g = test_lock().lock().unwrap();
    // Runs a write-file then finishes; existing WriteSummary contract holds.
    let mut s = Session::new(provider(vec![
        "```agnes\n(pipe (tool write-file :path \"/tmp/x\" :content \"hi\") finish)\n```",
    ]))
    .unwrap();
    let sink = RecordingSink::default();
    let sink = Arc::new(TokioMutex::new(sink));
    let _ = s
        .run_turn(TurnInput::NaturalLanguage("write it".into()), sink.clone())
        .await
        .unwrap();
    let evs = sink.lock().await.events();
    let pos_write = evs
        .iter()
        .position(|e| matches!(e, SessionEvent::WriteSummary { .. }));
    let pos_result = evs
        .iter()
        .position(|e| matches!(e, SessionEvent::TurnResult { .. }));
    let (pw, pr) = (
        pos_write.expect("WriteSummary emitted"),
        pos_result.expect("TurnResult emitted"),
    );
    assert!(pw < pr, "WriteSummary must precede TurnResult");
}

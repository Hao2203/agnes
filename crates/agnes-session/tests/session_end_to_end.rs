// These tests share the process-global `agnes_builtins::writes()` recorder,
// so they hold a `std::sync::Mutex` guard across await points to serialize.
// The pattern is safe here — one test is inside the critical section at a
// time and Session::run_turn does not itself take this lock — but clippy
// warns unconditionally, so opt out at the crate level.
#![allow(clippy::await_holding_lock)]

use agnes_llm::{MockProvider, Provider};
use agnes_session::{EventSink, Session, SessionEvent, TurnInput};
use std::sync::{Arc, Mutex, OnceLock};

struct CollectSink(pub Vec<SessionEvent>);

#[async_trait::async_trait]
impl EventSink for CollectSink {
    async fn emit(&mut self, ev: SessionEvent) {
        self.0.push(ev);
    }
}

/// All tests in this file share the process-global `agnes_builtins::writes()`
/// recorder. Serialize them so that one test's `run_turn` cannot drain
/// another test's write records while both are in flight.
fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn nl_turn_plans_and_executes_end_to_end() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    // Planner sees the goal and returns a DSL. Then translate/summarize
    // return canned strings. read-file uses the mocked in-memory table.
    let planner_response = "```agnes\n(pipe (tool read-file :path \"README.md\") (tool summarize))\n```";
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![
        planner_response.into(),       // planner call
        "one-sentence summary".into(), // summarize call
    ]));
    let mut session = Session::new(provider).unwrap();
    let mut sink = CollectSink(vec![]);
    let out = session
        .run_turn(
            TurnInput::NaturalLanguage("summarize the readme".into()),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(out.data.as_str().unwrap(), "one-sentence summary");

    // Sink event stream shape:
    let kinds: Vec<&str> = sink
        .0
        .iter()
        .map(|e| match e {
            SessionEvent::PlannerStart => "planner-start",
            SessionEvent::DslProduced { .. } => "dsl",
            SessionEvent::PlanReady { .. } => "plan",
            SessionEvent::NodeStart { .. } => "node-start",
            SessionEvent::NodeEnd { .. } => "node-end",
            SessionEvent::TurnResult { .. } => "turn-result",
            _ => "other",
        })
        .collect();
    assert!(kinds.contains(&"planner-start"));
    assert!(kinds.contains(&"dsl"));
    assert!(kinds.contains(&"plan"));
    assert!(
        kinds.iter().filter(|k| **k == "node-start").count() >= 2,
        "read-file + summarize expected"
    );
    assert!(kinds.iter().filter(|k| **k == "node-end").count() >= 2);
    assert!(kinds.contains(&"turn-result"));
}

#[tokio::test]
async fn raw_dsl_turn_skips_planner() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![])); // no planner calls
    let mut session = Session::new(provider).unwrap();
    let mut sink = CollectSink(vec![]);
    let out = session
        .run_turn(
            TurnInput::RawDsl("(tool read-file :path \"README.md\")".into()),
            &mut sink,
        )
        .await
        .unwrap();
    assert!(out.data.as_str().unwrap().contains("agnes"));
    // No PlannerStart event when RawDsl.
    assert!(
        !sink
            .0
            .iter()
            .any(|e| matches!(e, SessionEvent::PlannerStart))
    );
}

#[tokio::test]
async fn planner_retries_on_bad_dsl_then_succeeds() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![
        "```agnes\nBROKEN(\n```".into(),
        "```agnes\n(tool read-file :path \"README.md\")\n```".into(),
    ]));
    let mut session = Session::new(provider).unwrap();
    let mut sink = CollectSink(vec![]);
    let _ = session
        .run_turn(
            TurnInput::NaturalLanguage("read the readme".into()),
            &mut sink,
        )
        .await
        .expect("should recover on retry");
    let retry_count = sink
        .0
        .iter()
        .filter(|e| matches!(e, SessionEvent::PlannerRetry { .. }))
        .count();
    assert_eq!(retry_count, 1, "one retry expected");
}

#[tokio::test]
async fn raw_dsl_parse_error_emits_turn_failed() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    // Regression guard (I1): every failure surface — parse, check,
    // compile, retries-exhausted — must close the event stream with a
    // `TurnFailed` event; otherwise sinks are left dangling.
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));
    let mut session = Session::new(provider).unwrap();
    let mut sink = CollectSink(vec![]);
    let err = session
        .run_turn(TurnInput::RawDsl("(oops".into()), &mut sink)
        .await
        .expect_err("invalid DSL must return an error");
    let msg = err.to_string();
    assert!(msg.contains("parse"), "expected parse error, got: {msg}");
    let saw_failed = sink
        .0
        .iter()
        .any(|e| matches!(e, SessionEvent::TurnFailed { .. }));
    assert!(
        saw_failed,
        "parse-error path must emit TurnFailed; events: {:?}",
        sink.0
    );
}

#[tokio::test]
async fn write_file_turn_emits_write_summary() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    // Regression guard (I3): a turn that invokes `write-file` must
    // surface a `WriteSummary` event to the sink with the recorded
    // (path, byte-count) entries in call order.
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));
    let mut session = Session::new(provider).unwrap();
    let mut sink = CollectSink(vec![]);
    // Drain the process-global recorder before the turn so entries
    // from other tests do not contaminate this assertion.
    let _ = std::mem::take(
        &mut *agnes_builtins::writes()
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    let dsl = "(tool write-file :path \"/tmp/agnes-test.md\" :content \"hello\")";
    session
        .run_turn(TurnInput::RawDsl(dsl.into()), &mut sink)
        .await
        .unwrap();
    let summary = sink
        .0
        .iter()
        .find_map(|e| match e {
            SessionEvent::WriteSummary { entries } => Some(entries.clone()),
            _ => None,
        })
        .expect("WriteSummary event must be emitted");
    assert_eq!(summary, vec![("/tmp/agnes-test.md".to_string(), 5)]);
    // WriteSummary must fire BEFORE TurnResult so a sink can render it
    // as part of the turn's closing frame.
    let pos_summary = sink
        .0
        .iter()
        .position(|e| matches!(e, SessionEvent::WriteSummary { .. }))
        .unwrap();
    let pos_result = sink
        .0
        .iter()
        .position(|e| matches!(e, SessionEvent::TurnResult { .. }))
        .unwrap();
    assert!(
        pos_summary < pos_result,
        "WriteSummary must precede TurnResult"
    );
}

use agnes_llm::{MockProvider, Provider};
use agnes_session::{EventSink, Session, SessionEvent, TurnInput};
use std::sync::Arc;

struct CollectSink(pub Vec<SessionEvent>);

#[async_trait::async_trait]
impl EventSink for CollectSink {
    async fn emit(&mut self, ev: SessionEvent) {
        self.0.push(ev);
    }
}

#[tokio::test]
async fn nl_turn_plans_and_executes_end_to_end() {
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

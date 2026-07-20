//! Compile-time assertion: SessionEvent is #[non_exhaustive], meaning
//! external matches without a catchall arm will not compile. We can't
//! directly test that (it would need a proc-macro), so instead we
//! verify that a match with a catchall works AND that we can construct
//! variants normally.

use agnes_session::{NodeKindTag, SessionEvent};

#[test]
fn match_with_catchall_compiles_and_runs() {
    let ev = SessionEvent::TurnFailed { error: "x".into() };
    let s = match ev {
        SessionEvent::TurnFailed { error } => error,
        _ => "other".to_string(),
    };
    assert_eq!(s, "x");
}

#[test]
fn other_variants_still_constructible() {
    let _p = SessionEvent::PlannerStart;
    let _n = SessionEvent::NodeStart {
        id: 0,
        kind: NodeKindTag::Llm,
        args: vec![],
    };
    let _r = SessionEvent::TurnResult {
        value_preview: "".into(),
        value_type: "PlainText".into(),
    };
}

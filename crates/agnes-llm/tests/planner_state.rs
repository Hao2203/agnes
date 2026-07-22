use agnes_builtins::register_builtins;
use agnes_llm::{Iteration, MockProvider, Observation, Planner, Turn, TurnOutcome};
use agnes_registry::Registry;
use agnes_types::TypeName;
use std::sync::Arc;

fn reg() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

fn planner_with(responses: Vec<String>) -> Planner {
    let r = reg();
    Planner::new(Arc::new(MockProvider::new(responses)), &r)
}

#[test]
fn begin_user_turn_seeds_but_does_not_commit() {
    let mut p = planner_with(vec![]);
    p.begin_user_turn("Translate the file".into());
    // Before anything runs, history is still empty.
    assert!(p.history().is_empty());
}

#[tokio::test]
async fn plan_next_appends_assistant_dsl_to_inflight_iterations() {
    let mut p = planner_with(vec!["```agnes\n(pipe \"hi\" finish)\n```".into()]);
    p.begin_user_turn("say hi".into());
    let dsl = p.plan_next().await.unwrap();
    assert_eq!(dsl.trim(), "(pipe \"hi\" finish)");
    // Not yet committed.
    assert!(p.history().is_empty());
}

#[tokio::test]
async fn push_observation_attaches_to_last_iteration() {
    let mut p = planner_with(vec![
        "```agnes\n(pipe (tool summarize \"x\") observe)\n```".into(),
        "```agnes\n(pipe \"done\" finish)\n```".into(),
    ]);
    p.begin_user_turn("...".into());

    let dsl1 = p.plan_next().await.unwrap();
    p.push_observation(
        dsl1,
        "the summary".into(),
        false,
        Some(TypeName("Summary".into())),
    );

    // Still not committed.
    assert!(p.history().is_empty());

    // Second plan_next should include the observation as a `user` message
    // in the request. We verify that indirectly by driving another iteration.
    let dsl2 = p.plan_next().await.unwrap();
    assert!(dsl2.contains("finish"));
}

#[tokio::test]
async fn record_finish_commits_the_turn_with_finished_outcome() {
    let mut p = planner_with(vec!["```agnes\n(pipe \"ok\" finish)\n```".into()]);
    p.begin_user_turn("hi".into());
    let dsl = p.plan_next().await.unwrap();
    p.record_finish(dsl.clone(), "ok".into());
    let hist = p.history();
    assert_eq!(hist.len(), 1);
    let t: &Turn = &hist[0];
    assert_eq!(t.user_nl, "hi");
    assert_eq!(t.iterations.len(), 1);
    let it: &Iteration = &t.iterations[0];
    assert_eq!(it.assistant_dsl, dsl);
    assert!(
        it.observation.is_none(),
        "final iteration has no observation"
    );
    match &t.outcome {
        TurnOutcome::Finished { result } => assert_eq!(result, "ok"),
        other => panic!("expected Finished, got {other:?}"),
    }
}

#[tokio::test]
async fn abandon_pending_turn_stamps_turn_limit_exceeded() {
    let mut p = planner_with(vec![
        "```agnes\n(pipe \"a\" observe)\n```".into(),
        "```agnes\n(pipe \"b\" observe)\n```".into(),
    ]);
    p.begin_user_turn("won't finish".into());
    let d1 = p.plan_next().await.unwrap();
    p.push_observation(d1, "a".into(), false, None);
    let d2 = p.plan_next().await.unwrap();
    p.push_observation(d2, "b".into(), false, None);

    p.abandon_pending_turn();
    let hist = p.history();
    // abandon_pending_turn now commits the in-flight turn with a
    // TurnLimitExceeded outcome, so history grows by one.
    assert_eq!(hist.len(), 1);
    assert!(matches!(hist[0].outcome, TurnOutcome::TurnLimitExceeded));
    assert_eq!(hist[0].iterations.len(), 2);
}

#[test]
fn abandon_pending_turn_on_no_inflight_is_noop() {
    let mut p = planner_with(vec![]);
    p.abandon_pending_turn();
    assert!(p.history().is_empty());
}

#[test]
fn observation_records_type_name_when_provided() {
    let obs = Observation {
        text: "hello".into(),
        is_error: false,
        type_name: Some(TypeName("Summary".into())),
    };
    assert_eq!(obs.text, "hello");
    assert!(!obs.is_error);
    assert_eq!(obs.type_name.as_ref().unwrap().0, "Summary");
}

#[tokio::test]
async fn empty_response_error_carries_raw_preview() {
    use agnes_llm::PlannerError;
    // Fenced block with only whitespace inside — extract_dsl returns "".
    let raw = "```agnes\n   \n```".to_string();
    let mut p = planner_with(vec![raw.clone()]);
    p.begin_user_turn("...".into());
    let err = p.plan_next().await.unwrap_err();
    match err {
        PlannerError::EmptyResponse {
            raw_len,
            raw_preview,
        } => {
            assert_eq!(raw_len, raw.chars().count());
            assert!(
                raw_preview.contains("```agnes"),
                "preview should contain the fence: {raw_preview:?}"
            );
        }
        other => panic!("expected EmptyResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_response_labels_whitespace_only_raw() {
    use agnes_llm::PlannerError;
    let mut p = planner_with(vec!["   \n\n".into()]);
    p.begin_user_turn("...".into());
    let err = p.plan_next().await.unwrap_err();
    match err {
        PlannerError::EmptyResponse { raw_preview, .. } => {
            assert!(
                raw_preview.contains("whitespace-only"),
                "preview should flag whitespace-only: {raw_preview:?}"
            );
        }
        other => panic!("expected EmptyResponse, got {other:?}"),
    }
}

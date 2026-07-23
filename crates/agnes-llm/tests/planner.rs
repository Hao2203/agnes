//! Planner tests: system prompt discipline + message construction with the
//! new agent-loop state model. Round-trips go through MockProvider; no
//! real network.

use agnes_builtins::register_builtins;
use agnes_llm::{MockProvider, Planner, Provider, Role};
use agnes_registry::Registry;
use agnes_types::TypeName;
use std::sync::Arc;

fn reg() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

fn planner_with(responses: Vec<String>) -> (Planner, Arc<MockProvider>) {
    let r = reg();
    let mock = Arc::new(MockProvider::new(responses));
    let p = Planner::new(mock.clone() as Arc<dyn Provider>, &r);
    (p, mock)
}

#[tokio::test]
async fn system_prompt_lists_all_builtin_tools_and_mentions_finish_observe_forms() {
    // finish/observe are now special forms — they must NOT appear in the
    // tool catalog. They MUST still be documented as language forms.
    let (mut p, mock) = planner_with(vec!["```agnes\n(finish \"hi\")\n```".into()]);
    p.begin_user_turn("hi".into());
    let _ = p.plan_next().await.unwrap();
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let sys = seen[0].system.as_deref().unwrap_or("");
    // Every I/O tool must be catalogued.
    for name in &[
        "read-file",
        "write-file",
        "parse-path",
        "summarize",
        "translate",
        "llm",
        "join-lines",
        "shell-run",
    ] {
        assert!(sys.contains(name), "system prompt missing tool `{name}`");
    }
    // finish/observe must be described (they're special forms), and the
    // `(finish X)` / `(observe X)` shape must be shown.
    assert!(
        sys.contains("(finish X)") && sys.contains("(observe X)"),
        "system prompt must document the (finish X) / (observe X) special forms"
    );
    // No `- finish :` catalog line should exist (that was the old tool form).
    assert!(
        !sys.contains("- finish :"),
        "finish must not appear in the tool catalog"
    );
    assert!(
        !sys.contains("- observe :"),
        "observe must not appear in the tool catalog"
    );
}

#[tokio::test]
async fn system_prompt_mentions_finish_and_observation_semantics() {
    let (mut p, mock) = planner_with(vec!["```agnes\n(finish \"hi\")\n```".into()]);
    p.begin_user_turn("hi".into());
    let _ = p.plan_next().await.unwrap();
    let sys = mock.seen()[0].system.clone().unwrap_or_default();
    // The prompt must explain the loop protocol.
    assert!(
        sys.contains("finish") && sys.contains("observe"),
        "system prompt must reference finish and observe semantics"
    );
    assert!(
        sys.contains("<observation"),
        "system prompt must show LLM the <observation> block format"
    );
}

#[tokio::test]
async fn observation_message_uses_xml_wrapper_with_type_name() {
    let (mut p, mock) = planner_with(vec![
        "```agnes\n(pipe (tool summarize \"x\") observe)\n```".into(),
        "```agnes\n(pipe \"done\" finish)\n```".into(),
    ]);
    p.begin_user_turn("do it".into());
    let d1 = p.plan_next().await.unwrap();
    p.push_observation(
        d1,
        "the summary".into(),
        false,
        Some(TypeName("Summary".into())),
    );
    let _d2 = p.plan_next().await.unwrap();

    // Second request's second-to-last message should be a user message
    // wrapping the observation in XML with type="Summary".
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    let msgs2 = &seen[1].messages;
    let obs_msg = msgs2
        .iter()
        .find(|m| matches!(m.role, Role::User) && m.content.contains("<observation"))
        .expect("observation user message missing");
    assert!(
        obs_msg.content.contains("type=\"Summary\""),
        "observation message missing type=\"Summary\": {}",
        obs_msg.content
    );
    assert!(obs_msg.content.contains("the summary"));
}

#[tokio::test]
async fn error_observation_uses_error_true_attribute() {
    let (mut p, mock) = planner_with(vec![
        "```agnes\n(pipe (tool bogus) observe)\n```".into(),
        "```agnes\n(pipe \"ok\" finish)\n```".into(),
    ]);
    p.begin_user_turn("do it".into());
    let d1 = p.plan_next().await.unwrap();
    p.push_observation(d1, "parse: unknown tool 'bogus'".into(), true, None);
    let _ = p.plan_next().await.unwrap();

    let seen = mock.seen();
    let msgs2 = &seen[1].messages;
    let err_msg = msgs2
        .iter()
        .find(|m| matches!(m.role, Role::User) && m.content.contains("<observation"))
        .expect("error observation message missing");
    assert!(err_msg.content.contains("error=\"true\""));
    // Error observations MUST NOT include a type attribute.
    assert!(!err_msg.content.contains("type=\""));
    assert!(err_msg.content.contains("unknown tool 'bogus'"));
}

#[tokio::test]
async fn message_chain_alternates_roles_after_multiple_iterations() {
    // Regression guard: consecutive same-role messages break Anthropic API.
    // With observations interleaved, the chain must strictly alternate.
    let (mut p, mock) = planner_with(vec![
        "```agnes\n(pipe X observe)\n```".into(),
        "```agnes\n(pipe Y observe)\n```".into(),
        "```agnes\n(pipe \"done\" finish)\n```".into(),
    ]);
    p.begin_user_turn("try it".into());
    let d1 = p.plan_next().await.unwrap();
    p.push_observation(d1, "A".into(), false, None);
    let d2 = p.plan_next().await.unwrap();
    p.push_observation(d2, "B".into(), false, None);
    let _ = p.plan_next().await.unwrap();

    let seen = mock.seen();
    let last = &seen[seen.len() - 1].messages;
    // Roles must alternate: user, assistant, user, assistant, user, assistant, user.
    let roles: Vec<_> = last.iter().map(|m| m.role).collect();
    for pair in roles.windows(2) {
        assert_ne!(
            pair[0], pair[1],
            "consecutive same-role messages: {roles:?}"
        );
    }
    // And the last message before this LLM call must be a user (the observation).
    assert_eq!(*roles.last().unwrap(), Role::User);
}

#[tokio::test]
async fn committed_history_replays_in_subsequent_turns() {
    let (mut p, mock) = planner_with(vec![
        "```agnes\n(pipe \"first\" finish)\n```".into(),
        "```agnes\n(pipe \"second\" finish)\n```".into(),
    ]);
    p.begin_user_turn("turn 1".into());
    let d1 = p.plan_next().await.unwrap();
    p.record_finish(d1, "first".into());

    p.begin_user_turn("turn 2".into());
    let _ = p.plan_next().await.unwrap();

    let seen = mock.seen();
    let msgs2 = &seen[1].messages;
    // Should see turn 1's user_nl, assistant DSL, and turn 2's user_nl.
    let has_turn1_user = msgs2
        .iter()
        .any(|m| m.content == "turn 1" && matches!(m.role, Role::User));
    let has_turn1_assistant = msgs2
        .iter()
        .any(|m| m.content.contains("first") && matches!(m.role, Role::Assistant));
    let has_turn2_user = msgs2
        .iter()
        .any(|m| m.content == "turn 2" && matches!(m.role, Role::User));
    assert!(has_turn1_user, "history missing turn 1 user_nl");
    assert!(has_turn1_assistant, "history missing turn 1 assistant DSL");
    assert!(has_turn2_user, "history missing turn 2 user_nl");
}

#[tokio::test]
async fn old_turns_beyond_six_collapse_into_prior_context() {
    // Build 8 responses so we can commit 7 turns before the 8th; only
    // the last 6 should appear verbatim; the first 1 should be in the
    // prior-context prefix.
    let responses: Vec<String> = (0..8)
        .map(|i| format!("```agnes\n(pipe \"turn{i}\" finish)\n```"))
        .collect();
    let (mut p, mock) = planner_with(responses);
    for i in 0..7 {
        p.begin_user_turn(format!("nl {i}"));
        let d = p.plan_next().await.unwrap();
        p.record_finish(d, format!("result {i}"));
    }
    // 8th turn — the system prompt at this call should include the prior-
    // context prefix for turn 0 only.
    p.begin_user_turn("nl 7".into());
    let _ = p.plan_next().await.unwrap();
    let seen = mock.seen();
    let sys8 = seen[7].system.clone().unwrap_or_default();
    assert!(
        sys8.contains("<prior context:"),
        "system prompt missing prior-context prefix on 8th turn: {sys8}"
    );
    assert!(
        sys8.contains("nl 0"),
        "prior context should reference turn 0"
    );
    // But turn 1 (the second one) should NOT be summarized — it should
    // still be verbatim in messages.
    let msgs8 = &seen[7].messages;
    let has_nl1_user = msgs8
        .iter()
        .any(|m| m.content == "nl 1" && matches!(m.role, Role::User));
    assert!(has_nl1_user, "turn 1 should still be in messages verbatim");
}

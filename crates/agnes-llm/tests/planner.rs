use agnes_builtins::register_builtins;
use agnes_llm::{MockProvider, Planner, Provider};
use agnes_registry::Registry;
use std::sync::Arc;

fn reg_with_builtins() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

#[tokio::test]
async fn planner_returns_extracted_dsl() {
    let raw = "Sure:\n\n```agnes\n(tool read-file :path \"README.md\")\n```";
    let mock: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![raw.into()]));
    let reg = reg_with_builtins();
    let mut p = Planner::new(mock, &reg);
    let dsl = p.plan("read the readme").await.unwrap();
    assert_eq!(dsl, "(tool read-file :path \"README.md\")");
}

#[tokio::test]
async fn planner_system_prompt_lists_every_tool() {
    let mock = Arc::new(MockProvider::new(vec![
        "```agnes\n(tool read-file :path \"a\")\n```".into(),
    ]));
    let reg = reg_with_builtins();
    let mut p = Planner::new(mock.clone(), &reg);
    let _ = p.plan("do stuff").await.unwrap();
    let seen = mock.seen();
    let sys = seen[0].system.as_deref().unwrap();
    for name in [
        "read-file",
        "write-file",
        "summarize",
        "translate",
        "ocr",
        "llm",
        "join-lines",
    ] {
        assert!(sys.contains(name), "system prompt must list `{name}`; got: {sys}");
    }
}

#[tokio::test]
async fn planner_feeds_error_back_on_retry() {
    let mock = Arc::new(MockProvider::new(vec![
        "```agnes\nBROKEN\n```".into(),
        "```agnes\n(tool read-file :path \"README.md\")\n```".into(),
    ]));
    let reg = reg_with_builtins();
    let mut p = Planner::new(mock.clone(), &reg);

    let _ = p.plan("read readme").await.unwrap();
    p.push_error_feedback("BROKEN".into(), "syntax error at 1:1".into());
    let dsl2 = p.plan("read readme").await.unwrap();
    assert_eq!(dsl2, "(tool read-file :path \"README.md\")");

    let seen = mock.seen();
    let second = &seen[1];
    // The second call's message chain includes the previous bad DSL + the
    // "That failed with:" user turn.
    let chain: Vec<String> = second.messages.iter().map(|m| m.content.clone()).collect();
    let joined = chain.join("\n---\n");
    assert!(joined.contains("BROKEN"), "chain must carry the previous bad DSL; got: {joined}");
    assert!(joined.contains("That failed with"), "chain must carry the error hint; got: {joined}");
}

#[tokio::test]
async fn record_result_commits_a_turn_and_scratch_clears() {
    let mock = Arc::new(MockProvider::new(vec![
        "```agnes\n(tool ocr :source \"a.pdf\")\n```".into(),
    ]));
    let reg = reg_with_builtins();
    let mut p = Planner::new(mock, &reg);
    let _ = p.plan("ocr something").await.unwrap();
    p.record_result(
        "(tool ocr :source \"a.pdf\")".into(),
        "Extracted text: ...".into(),
    );
    let hist = p.history();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].user_nl, "ocr something");
    assert!(hist[0].assistant_dsl.contains("ocr"));
}

#[tokio::test]
async fn retry_chain_has_no_consecutive_same_role_turns() {
    // Regression guard: after `plan()` + `push_error_feedback()` + `plan()`,
    // the second request's `messages` must strictly alternate roles.
    // Anthropic's Messages API 400s on consecutive same-role turns.
    use agnes_llm::Role;
    let mock = Arc::new(MockProvider::new(vec![
        "```agnes\nBROKEN\n```".into(),
        "```agnes\n(tool read-file :path \"README.md\")\n```".into(),
    ]));
    let reg = reg_with_builtins();
    let mut p = Planner::new(mock.clone(), &reg);

    let _ = p.plan("read readme").await.unwrap();
    p.push_error_feedback("BROKEN".into(), "syntax error at 1:1".into());
    let _ = p.plan("read readme").await.unwrap();

    let seen = mock.seen();
    let second = &seen[1];
    let roles: Vec<Role> = second.messages.iter().map(|m| m.role).collect();
    for pair in roles.windows(2) {
        assert_ne!(
            pair[0], pair[1],
            "messages must strictly alternate roles; got: {roles:?}"
        );
    }
    // Sanity: the retry still carries the bad DSL and the error hint.
    let joined = second
        .messages
        .iter()
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n---\n");
    assert!(joined.contains("BROKEN"), "chain must carry bad DSL");
    assert!(
        joined.contains("That failed with"),
        "chain must carry error hint"
    );
}

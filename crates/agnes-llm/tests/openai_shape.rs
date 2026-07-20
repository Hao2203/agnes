use agnes_llm::{CompletionRequest, Message, OpenAiCompatProvider, Role};

#[test]
fn openai_body_folds_system_into_messages() {
    let p = OpenAiCompatProvider::new(
        "gpt-4o-mini".into(),
        "sk-test".into(),
        "https://api.openai.com/v1".into(),
        reqwest::Client::new(),
    );
    let req = CompletionRequest {
        system: Some("be terse".into()),
        messages: vec![Message {
            role: Role::User,
            content: "hi".into(),
        }],
        max_tokens: 64,
    };
    let body = p.build_body(&req);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["model"], "gpt-4o-mini");
    assert_eq!(v["max_tokens"], 64);
    let msgs = v["messages"].as_array().unwrap();
    // system folded in as the first message with role=system
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "be terse");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[1]["content"], "hi");
}

#[test]
fn openai_endpoint_appends_chat_completions() {
    let p = OpenAiCompatProvider::new(
        "m".into(),
        "k".into(),
        "https://ark.cn-beijing.volces.com/api/v3".into(),
        reqwest::Client::new(),
    );
    assert_eq!(
        p.endpoint(),
        "https://ark.cn-beijing.volces.com/api/v3/chat/completions"
    );
}

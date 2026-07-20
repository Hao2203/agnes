use agnes_llm::{AnthropicProvider, CompletionRequest, Message, Role};

#[test]
fn anthropic_body_has_expected_shape() {
    let p = AnthropicProvider::new(
        "claude-haiku-4-5".into(),
        "sk-test".into(),
        reqwest::Client::new(),
    );
    let req = CompletionRequest {
        system: Some("you are helpful".into()),
        messages: vec![
            Message {
                role: Role::User,
                content: "hi".into(),
            },
            Message {
                role: Role::Assistant,
                content: "hello".into(),
            },
            Message {
                role: Role::User,
                content: "again".into(),
            },
        ],
        max_tokens: 256,
    };
    let body = p.build_body(&req);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["model"], "claude-haiku-4-5");
    assert_eq!(v["max_tokens"], 256);
    assert_eq!(v["system"], "you are helpful");
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "hi");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[2]["role"], "user");
}

#[test]
fn anthropic_body_omits_system_when_none() {
    let p = AnthropicProvider::new("m".into(), "k".into(), reqwest::Client::new());
    let req = CompletionRequest {
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: "x".into(),
        }],
        max_tokens: 8,
    };
    let body = p.build_body(&req);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v.get("system").is_none(), "system must be absent when None");
}

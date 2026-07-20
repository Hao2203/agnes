use agnes_llm::{CompletionRequest, Message, MockProvider, Provider, Role};
use std::sync::Arc;

#[tokio::test]
async fn mock_provider_returns_queued_responses_in_order() {
    let p: Arc<dyn Provider> =
        Arc::new(MockProvider::new(vec!["hello".into(), "world".into()]));
    let req1 = CompletionRequest {
        system: None,
        messages: vec![Message { role: Role::User, content: "a".into() }],
        max_tokens: 128,
    };
    let req2 = CompletionRequest {
        system: None,
        messages: vec![Message { role: Role::User, content: "b".into() }],
        max_tokens: 128,
    };
    let r1 = p.complete(req1).await.unwrap();
    let r2 = p.complete(req2).await.unwrap();
    assert_eq!(r1, "hello");
    assert_eq!(r2, "world");
}

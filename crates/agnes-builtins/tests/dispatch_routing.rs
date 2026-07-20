use agnes_builtins::native_dispatch;
use agnes_llm::MockProvider;
use agnes_types::Value;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;

fn args(kvs: &[(&str, &str)]) -> HashMap<String, Value> {
    kvs.iter()
        .map(|(k, v)| {
            (
                (*k).into(),
                Value::typed(JsonValue::String((*v).into()), "PlainText"),
            )
        })
        .collect()
}

#[tokio::test]
async fn translate_routes_through_provider() {
    let mock = Arc::new(MockProvider::new(vec!["こんにちは".into()]));
    let d = native_dispatch(mock.clone());
    let out = (d["translate"])(args(&[("input", "hello world"), ("lang", "ja")]))
        .await
        .unwrap();
    assert_eq!(out.data.as_str().unwrap(), "こんにちは");
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let req = &seen[0];
    let sys = req.system.as_deref().unwrap_or("");
    assert!(
        sys.contains("translator"),
        "system prompt should identify translator, got: {sys}"
    );
    assert_eq!(req.messages.len(), 1);
    let user = &req.messages[0].content;
    assert!(
        user.contains("Translate to ja"),
        "user prompt should name target lang, got: {user}"
    );
    assert!(
        user.contains("hello world"),
        "user prompt should carry input, got: {user}"
    );
}

#[tokio::test]
async fn summarize_routes_through_provider() {
    let mock = Arc::new(MockProvider::new(vec!["one-para summary".into()]));
    let d = native_dispatch(mock.clone());
    let out = (d["summarize"])(args(&[("input", "long body...")]))
        .await
        .unwrap();
    assert_eq!(out.data.as_str().unwrap(), "one-para summary");
    assert_eq!(out.declared_type.to_string(), "Summary");
}

#[tokio::test]
async fn llm_routes_through_provider() {
    let mock = Arc::new(MockProvider::new(vec!["result".into()]));
    let d = native_dispatch(mock.clone());
    let out = (d["llm"])(args(&[("prompt", "answer this"), ("input", "context")]))
        .await
        .unwrap();
    assert_eq!(out.data.as_str().unwrap(), "result");
    let seen = mock.seen();
    assert!(seen[0].system.is_none(), "llm tool sends no system prompt");
    assert!(seen[0].messages[0].content.contains("answer this"));
    assert!(seen[0].messages[0].content.contains("context"));
}

#[tokio::test]
async fn read_file_returns_mock_content_for_known_and_placeholder_for_unknown() {
    let mock = Arc::new(MockProvider::new(vec![]));
    let d = native_dispatch(mock);
    let known = (d["read-file"])(args(&[("path", "README.md")]))
        .await
        .unwrap();
    assert!(
        known.data.as_str().unwrap().contains("agnes"),
        "seeded README should mention agnes"
    );

    let unknown = (d["read-file"])(args(&[("path", "does-not-exist.md")]))
        .await
        .unwrap();
    let s = unknown.data.as_str().unwrap();
    assert!(s.contains("[MOCK file at does-not-exist.md"), "got: {s}");
}

#[tokio::test]
async fn write_file_does_not_touch_disk_and_records_call() {
    use std::path::Path;
    let mock = Arc::new(MockProvider::new(vec![]));
    let d = native_dispatch(mock);
    let out = (d["write-file"])(args(&[
        ("path", "/tmp/definitely-not-created-by-mock-agnes.txt"),
        ("content", "abc"),
    ]))
    .await
    .unwrap();
    assert!(out.data.is_null(), "write-file returns Unit (null JSON)");
    assert!(
        !Path::new("/tmp/definitely-not-created-by-mock-agnes.txt").exists(),
        "mock write-file must not touch disk"
    );
}

#[tokio::test]
async fn ocr_returns_fixed_placeholder() {
    let mock = Arc::new(MockProvider::new(vec![]));
    let d = native_dispatch(mock);
    let out = (d["ocr"])(args(&[("source", "any.pdf")])).await.unwrap();
    let s = out.data.as_str().unwrap();
    assert!(!s.is_empty(), "ocr must return some canned sentence");
    assert_eq!(out.declared_type.to_string(), "PlainText");
}

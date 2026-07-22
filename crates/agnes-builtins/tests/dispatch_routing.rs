use agnes_builtins::{native_dispatch, PathResolver};
use agnes_llm::MockProvider;
use agnes_types::Value;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

struct DummyResolver;
impl PathResolver for DummyResolver {
    fn resolve_path<'a>(&'a self, _input: &'a str) -> agnes_builtins::BoxFuture<'a, Result<PathBuf, String>> {
        panic!("dummy resolver should not be called in this test");
    }
}

static DUMMY: DummyResolver = DummyResolver;

fn args(kvs: &[(&str, &str)]) -> HashMap<String, Value> {
    kvs.iter()
        .map(|(k, v)| {
            (
                (*k).into(),
                Value::typed(JsonValue::String((*v).into()), "String"),
            )
        })
        .collect()
}

/// Dispatch table wired with a mock provider that returns a canned string,
/// so tool-call tests (including `llm`) can exercise the `(tool …)` path.
fn dispatch() -> HashMap<String, agnes_builtins::ToolImpl> {
    native_dispatch(Arc::new(MockProvider::new(vec!["ok".into()])))
}

#[tokio::test]
async fn translate_routes_through_provider() {
    let mock = Arc::new(MockProvider::new(vec!["こんにちは".into()]));
    let d = native_dispatch(mock.clone());
    let out = d["translate"].call(args(&[("input", "hello world"), ("lang", "ja")]), &DUMMY)
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
    let out = d["summarize"].call(args(&[("input", "long body...")]), &DUMMY)
        .await
        .unwrap();
    assert_eq!(out.data.as_str().unwrap(), "one-para summary");
}

#[tokio::test]
async fn llm_routes_through_provider() {
    let mock = Arc::new(MockProvider::new(vec!["result".into()]));
    let d = native_dispatch(mock.clone());
    let out = d["llm"].call(args(&[("prompt", "answer this"), ("input", "context")]), &DUMMY)
        .await
        .unwrap();
    assert_eq!(out.data.as_str().unwrap(), "result");
    let seen = mock.seen();
    assert!(seen[0].system.is_none(), "llm tool sends no system prompt");
    assert!(seen[0].messages[0].content.contains("answer this"));
    assert!(seen[0].messages[0].content.contains("context"));
}

#[tokio::test]
async fn llm_is_callable_via_tool_form() {
    // After de-special-casing, `llm` is an ordinary tool reached through
    // `(tool llm "p" "")`. The mock provider is wired by the
    // existing `dispatch()` in this file; exercise it the same way read-file etc. are.
    let d = dispatch();
    let llm = d.get("llm").expect("llm tool registered");
    let out = llm.call(args(&[("prompt", "hi"), ("input", "")]), &DUMMY).await.unwrap();
    assert_eq!(out.declared_type.to_string(), "String");
}

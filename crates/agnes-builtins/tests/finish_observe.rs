use agnes_builtins::{native_dispatch, register_builtins};
use agnes_llm::MockProvider;
use agnes_registry::Registry;
use agnes_types::{TypeExpr, TypeName, Value};
use serde_json::{Value as JsonValue, json};
use std::collections::HashMap;
use std::sync::Arc;

fn dispatch() -> HashMap<String, agnes_builtins::ToolImpl> {
    let mock = Arc::new(MockProvider::new(vec![]));
    native_dispatch(mock)
}

fn reg() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

fn kwargs_with_input(v: Value) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert("input".to_string(), v);
    m
}

#[tokio::test]
async fn finish_wraps_upstream_type_as_finish() {
    let d = dispatch();
    let finish = d.get("finish").expect("finish tool registered");
    let upstream = Value {
        data: json!("done"),
        declared_type: TypeExpr::named("PlainText"),
    };
    let out = finish(kwargs_with_input(upstream)).await.unwrap();
    // Data unchanged.
    assert_eq!(out.data, JsonValue::String("done".to_string()));
    // declared_type wrapped as (Finish PlainText).
    assert_eq!(
        out.declared_type,
        TypeExpr::App {
            head: TypeName("Finish".into()),
            args: vec![TypeExpr::named("PlainText")],
        }
    );
}

#[tokio::test]
async fn observe_wraps_upstream_type_as_observation() {
    let d = dispatch();
    let observe = d.get("observe").expect("observe tool registered");
    let upstream = Value {
        data: json!({"tokens": 42}),
        declared_type: TypeExpr::named("JSON"),
    };
    let out = observe(kwargs_with_input(upstream)).await.unwrap();
    assert_eq!(out.data, json!({"tokens": 42}));
    assert_eq!(
        out.declared_type,
        TypeExpr::App {
            head: TypeName("Observation".into()),
            args: vec![TypeExpr::named("JSON")],
        }
    );
}

#[tokio::test]
async fn finish_wraps_already_wrapped_type_last_one_wins() {
    // (pipe X observe finish) — spec §12 "last one wins" semantics.
    // Runtime wraps sequentially; the outermost head is what Session sees.
    let d = dispatch();
    let observe = d.get("observe").unwrap();
    let finish = d.get("finish").unwrap();

    let upstream = Value {
        data: json!("hi"),
        declared_type: TypeExpr::named("PlainText"),
    };
    let after_observe = observe(kwargs_with_input(upstream)).await.unwrap();
    let after_finish = finish(kwargs_with_input(after_observe)).await.unwrap();

    // Outer is Finish, inner is Observation of PlainText.
    assert_eq!(
        after_finish.declared_type,
        TypeExpr::App {
            head: TypeName("Finish".into()),
            args: vec![TypeExpr::App {
                head: TypeName("Observation".into()),
                args: vec![TypeExpr::named("PlainText")],
            }],
        }
    );
}

#[test]
fn finish_tool_registered_with_unknown_signature() {
    let r = reg();
    let sig = r.tool_signature("finish").expect("finish registered");
    // requires: [("input", Unknown)]
    assert_eq!(sig.requires.len(), 1);
    assert_eq!(sig.requires[0].0, "input");
    assert_eq!(sig.requires[0].1, TypeExpr::named("Unknown"));
    // provides: Unknown
    assert_eq!(sig.provides, TypeExpr::named("Unknown"));
}

#[test]
fn observe_tool_registered_with_unknown_signature() {
    let r = reg();
    let sig = r.tool_signature("observe").expect("observe registered");
    assert_eq!(sig.requires.len(), 1);
    assert_eq!(sig.requires[0].0, "input");
    assert_eq!(sig.requires[0].1, TypeExpr::named("Unknown"));
    assert_eq!(sig.provides, TypeExpr::named("Unknown"));
}

#[test]
fn finish_and_observation_types_registered() {
    let _r = reg();
    // Types must be registered so (declare tool ...) syntax with (Finish _)
    // won't fail with UnknownName. Task 3 already lets resolve accept the
    // heads; register_type here makes them first-class names too.
    // The exact API check: registering again fails with NameConflict.
    let mut r2 = Registry::new();
    register_builtins(&mut r2).unwrap();
    let err = r2.register_type("Finish", None).unwrap_err();
    match err {
        agnes_registry::RegistryError::NameConflict { name, .. } => {
            assert_eq!(name, "Finish");
        }
        other => panic!("expected NameConflict, got {other:?}"),
    }
}
use agnes_registry::{Registry, RegistryError};
use agnes_types::{ShowFn, TypeExpr, TypeName, Value};
use serde_json::json;

fn show_string(v: &serde_json::Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn show_wrap(v: &serde_json::Value) -> String {
    format!("<<{}>>", v.as_str().unwrap_or(""))
}

#[test]
fn register_show_records_a_function() {
    let mut reg = Registry::new();
    reg.register_show("Widget", show_string as ShowFn).unwrap();
    let got = reg.show_of(&TypeName("Widget".into())).unwrap();
    assert_eq!(got(&json!("hi")), "hi");
}

#[test]
fn duplicate_show_rejects_second_registration() {
    let mut reg = Registry::new();
    reg.register_show("Widget", show_string as ShowFn).unwrap();
    let err = reg
        .register_show("Widget", show_wrap as ShowFn)
        .expect_err("second registration should fail");
    match err {
        RegistryError::DuplicateShow { name } => assert_eq!(name, "Widget"),
        other => panic!("expected DuplicateShow, got {other:?}"),
    }
}

#[test]
fn register_show_is_independent_of_type_registration() {
    let mut reg = Registry::new();
    // register_type is not called; register_show still succeeds.
    reg.register_show("Widget", show_string as ShowFn).unwrap();
    assert!(reg.show_of(&TypeName("Widget".into())).is_some());
}

#[test]
fn show_value_uses_registered_show_for_named_type() {
    let mut reg = Registry::new();
    reg.register_show("Widget", show_wrap as ShowFn).unwrap();
    let v = Value {
        data: json!("hello"),
        declared_type: TypeExpr::named("Widget"),
    };
    assert_eq!(reg.show_value(&v), "<<hello>>");
}

#[test]
fn show_value_falls_back_to_json_pretty_for_unregistered_type() {
    let reg = Registry::new();
    let v = Value {
        data: json!({"a": 1, "b": [2, 3]}),
        declared_type: TypeExpr::named("Unregistered"),
    };
    let out = reg.show_value(&v);
    // Pretty json contains the keys.
    assert!(out.contains("\"a\""));
    assert!(out.contains("\"b\""));
}

#[test]
fn show_value_recurses_into_list_using_element_show() {
    let mut reg = Registry::new();
    reg.register_show("Item", show_wrap as ShowFn).unwrap();
    let v = Value {
        data: json!(["a", "b", "c"]),
        declared_type: TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![TypeExpr::named("Item")],
        },
    };
    assert_eq!(reg.show_value(&v), "[<<a>>, <<b>>, <<c>>]");
}

#[test]
fn show_value_unwraps_finish_wrapper() {
    let mut reg = Registry::new();
    reg.register_show("Msg", show_string as ShowFn).unwrap();
    let v = Value {
        data: json!("done"),
        declared_type: TypeExpr::App {
            head: TypeName("Finish".into()),
            args: vec![TypeExpr::named("Msg")],
        },
    };
    assert_eq!(reg.show_value(&v), "done");
}

#[test]
fn show_value_unwraps_observation_wrapper() {
    let mut reg = Registry::new();
    reg.register_show("Msg", show_string as ShowFn).unwrap();
    let v = Value {
        data: json!("thinking..."),
        declared_type: TypeExpr::App {
            head: TypeName("Observation".into()),
            args: vec![TypeExpr::named("Msg")],
        },
    };
    assert_eq!(reg.show_value(&v), "thinking...");
}

#[test]
fn show_value_option_some_returns_inner_show() {
    let mut reg = Registry::new();
    reg.register_show("Msg", show_wrap as ShowFn).unwrap();
    // Option T after canonicalize_union is (| T Unit); still: show inner if data isn't null.
    let v = Value {
        data: json!("here"),
        declared_type: agnes_types::canonicalize_union([
            TypeExpr::named("Msg"),
            TypeExpr::named("Unit"),
        ]),
    };
    assert_eq!(reg.show_value(&v), "<<here>>");
}

#[test]
fn show_value_option_none_returns_empty_string() {
    let mut reg = Registry::new();
    reg.register_show("Msg", show_wrap as ShowFn).unwrap();
    let v = Value {
        data: json!(null),
        declared_type: agnes_types::canonicalize_union([
            TypeExpr::named("Msg"),
            TypeExpr::named("Unit"),
        ]),
    };
    assert_eq!(reg.show_value(&v), "");
}

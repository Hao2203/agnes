use agnes_builtins::register_builtins;
use agnes_registry::Registry;
use agnes_types::{TypeExpr, Value};
use serde_json::json;

fn reg_with_builtins() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

fn v(data: serde_json::Value, ty: &str) -> Value {
    Value {
        data,
        declared_type: TypeExpr::named(ty),
    }
}

#[test]
fn json_show_pretty_prints_object() {
    let r = reg_with_builtins();
    let out = r.show_value(&v(json!({"a": 1, "b": [true, null]}), "JSON"));
    assert!(out.contains("\"a\""));
    assert!(out.contains("\"b\""));
    assert!(out.contains('\n'), "pretty print should include newlines");
}

#[test]
fn path_and_string_show_raw() {
    let r = reg_with_builtins();
    assert_eq!(r.show_value(&v(json!("/tmp/x"), "Path")), "/tmp/x");
    assert_eq!(r.show_value(&v(json!("abc"), "String")), "abc");
}

#[test]
fn int_and_bool_show_via_to_string() {
    let r = reg_with_builtins();
    assert_eq!(r.show_value(&v(json!(42), "Int")), "42");
    assert_eq!(r.show_value(&v(json!(true), "Bool")), "true");
    assert_eq!(r.show_value(&v(json!(false), "Bool")), "false");
}

#[test]
fn unit_show_is_empty_string() {
    let r = reg_with_builtins();
    assert_eq!(r.show_value(&v(json!(null), "Unit")), "");
    // Even non-null data still renders empty for Unit.
    assert_eq!(r.show_value(&v(json!("stuff"), "Unit")), "");
}

#[test]
fn list_of_string_shows_bracketed_comma_joined() {
    let r = reg_with_builtins();
    let ty = TypeExpr::App {
        head: agnes_types::TypeName("List".into()),
        args: vec![TypeExpr::named("String")],
    };
    let v = Value {
        data: json!(["a", "b", "c"]),
        declared_type: ty,
    };
    assert_eq!(r.show_value(&v), "[a, b, c]");
}

#[test]
fn duplicate_registration_fails() {
    // register_builtins is idempotent-unfriendly by design; calling twice
    // hits DuplicateShow (or NameConflict on types). Confirm the show side.
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let err = register_builtins(&mut r).unwrap_err();
    // Could be either NameConflict (types re-registered first) or
    // DuplicateShow (if types happened to succeed). Both are acceptable
    // - we just check the second call refuses cleanly.
    let msg = format!("{err}");
    assert!(!msg.is_empty(), "error message must be non-empty");
}

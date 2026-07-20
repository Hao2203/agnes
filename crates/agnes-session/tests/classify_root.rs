use agnes_session::{RootKind, classify_root, extract_inner_type};
use agnes_types::{TypeExpr, TypeName, Value};
use serde_json::json;

fn v(t: TypeExpr) -> Value {
    Value {
        data: json!(null),
        declared_type: t,
    }
}

#[test]
fn plain_named_type_is_other() {
    assert!(matches!(
        classify_root(&v(TypeExpr::named("PlainText"))),
        RootKind::Other
    ));
}

#[test]
fn finish_wrapper_is_finish() {
    let t = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert!(matches!(classify_root(&v(t)), RootKind::Finish));
}

#[test]
fn observation_wrapper_is_observation() {
    let t = TypeExpr::App {
        head: TypeName("Observation".into()),
        args: vec![TypeExpr::named("Summary")],
    };
    assert!(matches!(classify_root(&v(t)), RootKind::Observation));
}

#[test]
fn list_of_plaintext_is_other() {
    let t = TypeExpr::App {
        head: TypeName("List".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert!(matches!(classify_root(&v(t)), RootKind::Other));
}

#[test]
fn union_type_is_other() {
    // (| PlainText Markdown)
    let t = agnes_types::canonicalize_union([
        TypeExpr::named("PlainText"),
        TypeExpr::named("Markdown"),
    ]);
    assert!(matches!(classify_root(&v(t)), RootKind::Other));
}

#[test]
fn extract_inner_from_finish_returns_the_inner_name() {
    let t = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("Summary")],
    };
    assert_eq!(extract_inner_type(&t), Some(TypeName("Summary".into())));
}

#[test]
fn extract_inner_from_observation_of_list_returns_list_name() {
    // extract_inner_type only unwraps the OUTER Finish/Observation; the
    // inner type is whatever comes next. For an App head like List, we
    // return the head name ("List"), because that's what the XML attribute
    // needs — a stringy label the LLM can key off of.
    let t = TypeExpr::App {
        head: TypeName("Observation".into()),
        args: vec![TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![TypeExpr::named("PlainText")],
        }],
    };
    assert_eq!(extract_inner_type(&t), Some(TypeName("List".into())));
}

#[test]
fn extract_inner_from_named_finish_of_plaintext() {
    let t = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert_eq!(extract_inner_type(&t), Some(TypeName("PlainText".into())));
}

#[test]
fn extract_inner_from_non_wrapper_is_none() {
    assert!(extract_inner_type(&TypeExpr::named("PlainText")).is_none());
    let t = TypeExpr::App {
        head: TypeName("List".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert!(extract_inner_type(&t).is_none());
}

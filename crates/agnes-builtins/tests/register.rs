use agnes_builtins::{native_dispatch, register_builtins};
use agnes_registry::Registry;
use agnes_types::{TypeExpr, TypeName, type_expr_matches};
use std::sync::Arc;

fn reg() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).expect("builtins load");
    r
}

#[test]
fn registers_all_builtins() {
    let mut r = Registry::new();
    register_builtins(&mut r).expect("builtins load");
    assert!(r.tool_signature("read-file").is_some());
    assert!(r.tool_signature("write-file").is_some());
    assert!(r.tool_signature("summarize").is_some());
    assert!(r.tool_signature("translate").is_some());
    assert!(r.tool_signature("llm").is_some());
    assert!(r.tool_signature("parse-path").is_some());
}

#[test]
fn native_dispatch_has_all_impls() {
    let mock = Arc::new(agnes_llm::MockProvider::new(vec![]));
    let d = native_dispatch(mock);
    for name in [
        "read-file",
        "write-file",
        "summarize",
        "translate",
        "llm",
    ] {
        assert!(d.contains_key(name), "missing impl for {name}");
    }
}

#[test]
fn join_lines_registered() {
    let mut r = Registry::new();
    register_builtins(&mut r).expect("builtins load");
    assert!(r.tool_signature("join-lines").is_some());
}

#[test]
fn native_dispatch_has_join_lines() {
    let mock = Arc::new(agnes_llm::MockProvider::new(vec![]));
    let d = native_dispatch(mock);
    assert!(d.contains_key("join-lines"));
}

#[test]
fn join_lines_accepts_list_of_strings() {
    // The signature must be List String so a list of string literals
    // type-checks (the original web-server failure).
    let r = reg();
    let sig = r.tool_signature("join-lines").expect("join-lines registered");
    let lines_ty = &sig.requires[0].1;
    // lines_ty must be (List String): a String literal must satisfy it.
    let string_list = TypeExpr::App {
        head: TypeName("List".into()),
        args: vec![TypeExpr::named("String")],
    };
    assert!(
        type_expr_matches(&string_list, lines_ty),
        "join-lines :lines should accept (List String), got {lines_ty}"
    );
}

#[test]
fn removed_types_are_not_registered() {
    let r = reg();
    for gone in ["PlainText", "Markdown", "HTML", "Summary", "PDF", "Image", "TextLike", "VisualDoc"] {
        assert!(r.resolve(&agnes_ast::TypeExprAst::Named(gone.into())).is_err(),
            "type {gone} should no longer be registered");
    }
}

#[test]
fn ocr_is_not_registered() {
    let r = reg();
    assert!(r.tool_signature("ocr").is_none(), "ocr must be removed");
}

use agnes_checker::check;
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName};

fn seed_registry() -> Registry {
    let mut r = Registry::new();
    r.register_type("Path", None).unwrap();
    r.register_type("PlainText", None).unwrap();
    r.register_type("Markdown", None).unwrap();
    r.register_type("PDF", None).unwrap();
    r.register_type("Image", None).unwrap();
    r.register_type("Summary", None).unwrap();
    r.register_type("Unit", None).unwrap();
    r.register_type("String", None).unwrap();
    // Tools
    r.register_tool("read-file", ToolSignature {
        requires: vec![("path".into(), TypeExpr::Named(TypeName("Path".into())))],
        provides: TypeExpr::Named(TypeName("PlainText".into())),
    }).unwrap();
    let text_like = TypeExpr::Union([
        TypeName("PlainText".into()),
        TypeName("Markdown".into()),
    ].into_iter().collect());
    r.register_tool("summarize", ToolSignature {
        requires: vec![("input".into(), text_like.clone())],
        provides: TypeExpr::Named(TypeName("Summary".into())),
    }).unwrap();
    r.register_tool("ocr", ToolSignature {
        requires: vec![("source".into(), TypeExpr::Union([
            TypeName("PDF".into()),
            TypeName("Image".into()),
        ].into_iter().collect()))],
        provides: TypeExpr::Named(TypeName("PlainText".into())),
    }).unwrap();
    r
}

#[test]
fn happy_path_read_then_summarize() {
    let src = r#"(pipe (tool read-file :path "x") (tool summarize))"#;
    let p = parse(src).unwrap();
    let r = seed_registry();
    check(&p, &r).expect("should type-check");
}

#[test]
fn flow_mismatch_produces_llm_friendly_error() {
    let src = r#"(pipe (tool read-file :path "x.md") (tool ocr))"#;
    let p = parse(src).unwrap();
    let r = seed_registry();
    let err = check(&p, &r).unwrap_err();
    insta::assert_snapshot!("flow_mismatch", format!("{err}"));
}

#[test]
fn unknown_tool_reports() {
    let src = r#"(tool no-such-tool)"#;
    let p = parse(src).unwrap();
    let r = seed_registry();
    let err = check(&p, &r).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Unknown tool"), "got: {msg}");
    assert!(msg.contains("no-such-tool"), "got: {msg}");
}

use agnes_compiler::{CompileError, compile};
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName};

fn seed() -> Registry {
    let mut r = Registry::new();
    r.register_type("Path", None).unwrap();
    r.register_type("PlainText", None).unwrap();
    r.register_type("Summary", None).unwrap();
    r.register_tool(
        "read-file",
        ToolSignature {
            requires: vec![("path".into(), TypeExpr::Named(TypeName("Path".into())))],
            provides: TypeExpr::Named(TypeName("PlainText".into())),
        },
    )
    .unwrap();
    r.register_tool(
        "summarize",
        ToolSignature {
            requires: vec![("input".into(), TypeExpr::Named(TypeName("PlainText".into())))],
            provides: TypeExpr::Named(TypeName("Summary".into())),
        },
    )
    .unwrap();
    r
}

#[test]
fn compiles_a_pipe() {
    let src = r#"(pipe (tool read-file :path "x") (tool summarize))"#;
    let p = parse(src).unwrap();
    let r = seed();
    let dag = compile(&p, &r).expect("compile ok");
    assert!(dag.nodes.len() >= 2);
}

#[test]
fn detects_recursive_define() {
    let src = r#"
        (define loopy :params [] :provides Unit (tool loopy))
    "#;
    let mut r = seed();
    r.register_type("Unit", None).unwrap();
    let p = parse(src).unwrap();
    let err = compile(&p, &r).unwrap_err();
    match err {
        CompileError::CycleDetected { name } => assert_eq!(name, "loopy"),
        other => panic!("expected CycleDetected, got {other:?}"),
    }
}

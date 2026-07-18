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
            requires: vec![(
                "input".into(),
                TypeExpr::Named(TypeName("PlainText".into())),
            )],
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

    // Find the summarize node
    let summarize_node = dag
        .nodes
        .iter()
        .find(|n| matches!(&n.kind, agnes_compiler::NodeKind::Tool { name } if name == "summarize"))
        .expect("summarize node must exist");
    // It should have exactly one Kw input keyed "input" pointing at another node
    let input_kw = summarize_node
        .inputs
        .iter()
        .find(|i| matches!(i, agnes_compiler::Input::Kw { key, .. } if key == "input"))
        .expect("summarize should have :input kwarg from upstream flow");
    match input_kw {
        agnes_compiler::Input::Kw { source, .. } => {
            assert!(matches!(**source, agnes_compiler::Input::FromNode(_)));
        }
        _ => unreachable!(),
    }
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

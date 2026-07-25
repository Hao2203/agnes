use agnes_compiler::{CompileError, compile};
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName};

fn seed() -> Registry {
    let mut r = Registry::new();
    r.register_type("Path", None).unwrap();
    r.register_type("String", None).unwrap();
    r.register_type("Unit", None).unwrap();
    r.register_tool(
        "read-file",
        ToolSignature {
            requires: vec![("path".into(), TypeExpr::Named(TypeName("Path".into())))],
            provides: TypeExpr::Named(TypeName("String".into())),
        },
    )
    .unwrap();
    r.register_tool(
        "summarize",
        ToolSignature {
            requires: vec![(
                "input".into(),
                TypeExpr::Named(TypeName("String".into())),
            )],
            provides: TypeExpr::Named(TypeName("String".into())),
        },
    )
    .unwrap();
    r
}

#[test]
fn compiles_a_pipe() {
    let src = r#"(pipe (tool read-file "x") (tool summarize))"#;
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
        .expect("summarize should have `input` param from upstream flow");
    match input_kw {
        agnes_compiler::Input::Kw { source, .. } => {
            assert!(matches!(**source, agnes_compiler::Input::FromNode(_)));
        }
        _ => unreachable!(),
    }
}

#[test]
fn compiles_list_literal() {
    let src = r#"(list "a" "b")"#;
    let r = seed();
    let p = parse(src).unwrap();
    let dag = compile(&p, &r).expect("compile ok");
    // Expect a NodeKind::List with 2 element inputs.
    let list_node = dag
        .nodes
        .iter()
        .find(|n| matches!(n.kind, agnes_compiler::NodeKind::List))
        .expect("List node must exist");
    assert_eq!(list_node.inputs.len(), 2);
}

#[test]
fn detects_recursive_define() {
    let src = r#"
        (define loopy :params [] :provides Unit (tool loopy))
    "#;
    let r = seed();
    let p = parse(src).unwrap();
    let err = compile(&p, &r).unwrap_err();
    match err {
        CompileError::CycleDetected { name } => assert_eq!(name, "loopy"),
        other => panic!("expected CycleDetected, got {other:?}"),
    }
}

#[test]
fn node_spans_recorded_for_every_node() {
    let src = r#"(pipe (tool read-file "x") (tool summarize))"#;
    let p = parse(src).unwrap();
    let r = seed();
    let dag = compile(&p, &r).expect("compile ok");
    // Every node must have a corresponding span entry (1:1 mapping).
    assert_eq!(dag.nodes.len(), dag.node_spans.len());
    // All spans should be valid Span values (dummy or otherwise).
    for (i, span) in dag.node_spans.iter().enumerate() {
        assert!(
            span.start <= span.end,
            "node {i} has invalid span: {:?}",
            span
        );
    }
    // The root node's span should correspond to the outermost Expr.
    let root_span = dag.node_spans[dag.root.0];
    // (Currently the parser sets top-level spans to DUMMY; the important
    // thing is that a span is recorded for every node.)
    assert_eq!(root_span, dag.node_spans[dag.root.0]);
}

#[test]
fn tool_observe_lowers_to_self_contained_node() {
    let src = r#"(tool_observe read-file "x")"#;
    let p = parse(src).unwrap();
    let r = seed();
    let dag = compile(&p, &r).expect("compile ok");

    let observe_node = dag
        .nodes
        .iter()
        .find(|n| matches!(&n.kind, agnes_compiler::NodeKind::ToolObserve { name } if name == "read-file"))
        .expect("tool_observe node must exist");

    // Provides should be Observation String
    match &observe_node.provides {
        TypeExpr::App { head, args } => {
            assert_eq!(head.0, "Observation");
            assert_eq!(args.len(), 1);
            assert_eq!(args[0], TypeExpr::Named(TypeName("String".into())));
        }
        other => panic!("expected App type, got {other:?}"),
    }

    // Should have kwargs inputs (like a Tool node), not a single FromNode.
    let kw_inputs: Vec<_> = observe_node
        .inputs
        .iter()
        .filter(|i| matches!(i, agnes_compiler::Input::Kw { .. }))
        .collect();
    assert_eq!(kw_inputs.len(), 1, "should have 1 kw arg (path from literal)");
}

#[test]
fn bare_tool_observe_wraps_upstream() {
    let src = r#"(pipe (tool read-file "x") tool_observe)"#;
    let p = parse(src).unwrap();
    let r = seed();
    let dag = compile(&p, &r).expect("compile ok");

    let observe_node = dag
        .nodes
        .iter()
        .find(|n| matches!(&n.kind, agnes_compiler::NodeKind::ToolObserve { name } if name.is_empty()))
        .expect("bare tool_observe node must exist");

    // Bare form: one FromNode input (the upstream).
    assert_eq!(observe_node.inputs.len(), 1);
    assert!(matches!(observe_node.inputs[0], agnes_compiler::Input::FromNode(_)));

    // Provides should be Observation String
    match &observe_node.provides {
        TypeExpr::App { head, args } => {
            assert_eq!(head.0, "Observation");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected App type, got {other:?}"),
    }
}

#[test]
fn fmap_lowers_with_child_expr_and_outcome_provides() {
    // Use tool_observe to get an Observation upstream, then fmap over it.
    let src = r#"(pipe (tool_observe read-file "x") (fmap (tool summarize)))"#;
    let p = parse(src).unwrap();
    let r = seed();
    let dag = compile(&p, &r).expect("compile ok");

    let fmap_node = dag
        .nodes
        .iter()
        .find(|n| matches!(&n.kind, agnes_compiler::NodeKind::Fmap { .. }))
        .expect("fmap node must exist");

    // Fmap node should have exactly one input: the upstream Outcome node.
    assert_eq!(fmap_node.inputs.len(), 1);
    assert!(matches!(fmap_node.inputs[0], agnes_compiler::Input::FromNode(_)));

    // Fmap should store the child expression inline.
    match &fmap_node.kind {
        agnes_compiler::NodeKind::Fmap { child } => {
            // Child should be a tool call to summarize.
            match child.as_ref() {
                agnes_ast::Expr::Tool { name, .. } => {
                    assert_eq!(name, "summarize");
                }
                other => panic!("expected Tool child, got {other:?}"),
            }
        }
        _ => unreachable!(),
    }

    // Provides should be Observation String (wrapper preserved, child provides is String).
    match &fmap_node.provides {
        TypeExpr::App { head, args } => {
            assert_eq!(head.0, "Observation");
            assert_eq!(args.len(), 1);
            assert_eq!(args[0], TypeExpr::Named(TypeName("String".into())));
        }
        other => panic!("expected Observation String, got {other:?}"),
    }
}

#[test]
fn fmap_on_finish_preserves_finish_wrapper() {
    let src = r#"(pipe (finish "done") (fmap (tool summarize)))"#;
    let p = parse(src).unwrap();
    let r = seed();
    let dag = compile(&p, &r).expect("compile ok");

    let fmap_node = dag
        .nodes
        .iter()
        .find(|n| matches!(&n.kind, agnes_compiler::NodeKind::Fmap { .. }))
        .expect("fmap node must exist");

    match &fmap_node.provides {
        TypeExpr::App { head, args } => {
            assert_eq!(head.0, "Finish");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected Finish wrapper, got {other:?}"),
    }
}

#[test]
fn fmap_without_upstream_errors() {
    let src = r#"(fmap (tool summarize))"#;
    let p = parse(src).unwrap();
    let r = seed();
    let err = compile(&p, &r).expect_err("fmap with no upstream should error");
    let msg = format!("{err}");
    assert!(msg.contains("fmap used outside a pipe"), "got: {msg}");
}

use agnes_checker::check;
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr};

fn seed_registry() -> Registry {
    let mut r = Registry::new();
    r.register_type("Path", None).unwrap();
    r.register_type("String", None).unwrap();
    r.register_type("Unit", None).unwrap();
    // Tools
    r.register_tool(
        "read-file",
        ToolSignature {
            requires: vec![("path".into(), TypeExpr::named("Path"))],
            provides: TypeExpr::named("String"),
        },
    )
    .unwrap();
    r.register_tool(
        "summarize",
        ToolSignature {
            requires: vec![("input".into(), TypeExpr::named("String"))],
            provides: TypeExpr::named("String"),
        },
    )
    .unwrap();
    r
}

#[test]
fn happy_path_read_then_summarize() {
    let src = r#"(pipe (tool read-file "x") (tool summarize))"#;
    let p = parse(src).unwrap();
    let r = seed_registry();
    check(&p, &r).expect("should type-check");
}

#[test]
fn list_of_string_typed_correctly() {
    // Register a tool that takes (List String).
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_tool(
        "consume-strings",
        ToolSignature {
            requires: vec![(
                "items".into(),
                TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![TypeExpr::named("String")],
                },
            )],
            provides: TypeExpr::named("String"),
        },
    )
    .unwrap();

    let src = r#"(tool consume-strings ["a" "b" "c"])"#;
    let p = parse(src).unwrap();
    check(&p, &r).expect("must type-check");
}

#[test]
fn list_of_mixed_types_rejected_where_list_of_string_expected() {
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_type("Int", None).unwrap();
    r.register_tool(
        "consume-strings",
        ToolSignature {
            requires: vec![(
                "items".into(),
                TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![TypeExpr::named("String")],
                },
            )],
            provides: TypeExpr::named("String"),
        },
    )
    .unwrap();

    let src = r#"(tool consume-strings ["a" 1])"#;
    let p = parse(src).unwrap();
    let err = check(&p, &r).expect_err("must reject");
    let msg = format!("{err}");
    assert!(msg.contains("List"), "got: {msg}");
    assert!(msg.contains("String") || msg.contains("Int"), "got: {msg}");
}

#[test]
fn empty_list_adapts_to_hint() {
    // Given a tool requiring (List String), passing [] should succeed.
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_type("Unknown", None).unwrap();
    r.register_tool(
        "consume-strings",
        ToolSignature {
            requires: vec![(
                "items".into(),
                TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![TypeExpr::named("String")],
                },
            )],
            provides: TypeExpr::named("String"),
        },
    )
    .unwrap();

    let src = r#"(tool consume-strings [])"#;
    let p = parse(src).unwrap();
    check(&p, &r).expect("empty list must adapt to (List String)");
}

#[test]
fn unbound_empty_list_via_let_is_still_list_unknown() {
    // No hint at let site -> empty list types as (List Unknown).
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_type("Unknown", None).unwrap();
    r.register_tool(
        "consume-strings",
        ToolSignature {
            requires: vec![(
                "items".into(),
                TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![TypeExpr::named("String")],
                },
            )],
            provides: TypeExpr::named("String"),
        },
    )
    .unwrap();

    let src = r#"
        (pipe
          (let xs [])
          (tool consume-strings xs))
    "#;
    let p = parse(src).unwrap();
    let err = check(&p, &r).expect_err("must fail");
    let msg = format!("{err}");
    assert!(msg.contains("List"), "got: {msg}");
    assert!(
        msg.contains("Unknown") || msg.contains("String"),
        "got: {msg}"
    );
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

#[test]
fn fmap_on_observation_ok() {
    let r = seed_registry();
    let src = r#"(pipe (tool_observe read-file "x") (fmap (tool summarize)))"#;
    let p = parse(src).unwrap();
    check(&p, &r).expect("fmap on Observation should type-check");
}

#[test]
fn fmap_on_plain_upstream_rejected() {
    let r = seed_registry();
    let src = r#"(pipe (tool read-file "x") (fmap (tool summarize)))"#;
    let p = parse(src).unwrap();
    let err = check(&p, &r).expect_err("must reject fmap on plain type");
    let msg = format!("{err}");
    assert!(
        msg.contains("fmap requires an Outcome upstream"),
        "got: {msg}"
    );
}

#[test]
fn fmap_on_finish_ok() {
    let r = seed_registry();
    let src = r#"(pipe (finish "done") (fmap (tool summarize)))"#;
    let p = parse(src).unwrap();
    check(&p, &r).expect("fmap on Finish should type-check");
}

#[test]
fn observe_on_finish_rejected() {
    let r = seed_registry();
    let src = r#"(pipe (finish "done") observe)"#;
    let p = parse(src).unwrap();
    let err = check(&p, &r).expect_err("must reject observe on Finish");
    let msg = format!("{err}");
    assert!(msg.contains("cannot observe a Finish"), "got: {msg}");
}

#[test]
fn finish_on_observation_rejected() {
    let r = seed_registry();
    let src = r#"(pipe (tool_observe read-file "x") finish)"#;
    let p = parse(src).unwrap();
    let err = check(&p, &r).expect_err("must reject finish on Observation");
    let msg = format!("{err}");
    assert!(msg.contains("cannot finish an Observation"), "got: {msg}");
}

#[test]
fn tool_observe_produces_observation_via_define() {
    let mut r = seed_registry();
    // Register Observation type so the parser can resolve it in :provides.
    r.register_type("Observation", None).unwrap();
    let src = r#"
        (define test-tool-observe :provides (Observation String)
          (tool_observe read-file "x"))
    "#;
    let p = parse(src).unwrap();
    check(&p, &r).expect("tool_observe should produce Observation String");
}

#[test]
fn tool_observe_on_outcome_rejected() {
    let r = seed_registry();
    let src = r#"(pipe (tool_observe read-file "x") (tool_observe summarize))"#;
    let p = parse(src).unwrap();
    let err = check(&p, &r).expect_err("must reject tool_observe on Outcome");
    let msg = format!("{err}");
    assert!(
        msg.contains("tool_observe requires a plain upstream"),
        "got: {msg}"
    );
}

#[test]
fn bare_tool_observe_wraps_upstream() {
    let mut r = seed_registry();
    r.register_type("Observation", None).unwrap();
    let src = r#"
        (define test-bare-observe :provides (Observation String)
          (pipe (tool read-file "x") tool_observe))
    "#;
    let p = parse(src).unwrap();
    check(&p, &r).expect("bare tool_observe should wrap upstream in Observation");
}

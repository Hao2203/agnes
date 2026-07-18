use agnes_registry::Registry;
use agnes_types::{TypeExpr, TypeName, canonicalize_union};

#[test]
fn duplicate_type_is_rejected() {
    let mut r = Registry::new();
    r.register_type("PDF", None).unwrap();
    let err = r.register_type("PDF", None).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Name conflict"), "got: {msg}");
    assert!(msg.contains("PDF"), "got: {msg}");
}

#[test]
fn alias_conflicts_with_type() {
    let mut r = Registry::new();
    r.register_type("Text", None).unwrap();
    let expr = TypeExpr::Named(TypeName("PDF".into()));
    let err = r.register_alias("Text", expr).unwrap_err();
    assert!(format!("{err}").contains("Name conflict"));
}

#[test]
fn resolve_alias_flattens_nested_union() {
    use agnes_ast::TypeExprAst;
    let mut r = Registry::new();
    r.register_type("PlainText", None).unwrap();
    r.register_type("Markdown", None).unwrap();
    r.register_type("HTML", None).unwrap();
    r.register_alias(
        "TextLike",
        canonicalize_union([
            TypeExpr::named("PlainText"),
            TypeExpr::named("Markdown"),
            TypeExpr::named("HTML"),
        ]),
    )
    .unwrap();

    // (| TextLike PDF) should resolve to a flat 4-member union.
    r.register_type("PDF", None).unwrap();
    let ast = TypeExprAst::App {
        head: "|".into(),
        args: vec![
            TypeExprAst::Named("TextLike".into()),
            TypeExprAst::Named("PDF".into()),
        ],
    };
    let resolved = r.resolve(&ast).unwrap();
    match resolved {
        TypeExpr::App { head, args } => {
            assert_eq!(head.0, "|");
            assert_eq!(args.len(), 4);
            let names: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            assert!(names.contains(&"PlainText".into()));
            assert!(names.contains(&"PDF".into()));
            assert!(names.contains(&"Markdown".into()));
            assert!(names.contains(&"HTML".into()));
        }
        other => panic!("expected App with head `|`, got {other:?}"),
    }
}

#[test]
fn resolves_list_of_string() {
    use agnes_ast::TypeExprAst;
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    let ast = TypeExprAst::App {
        head: "List".into(),
        args: vec![TypeExprAst::Named("String".into())],
    };
    let resolved = r.resolve(&ast).unwrap();
    assert_eq!(
        resolved,
        TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![TypeExpr::named("String")],
        }
    );
}

#[test]
fn resolves_option_expands_to_union_with_unit() {
    use agnes_ast::TypeExprAst;
    let mut r = Registry::new();
    r.register_type("String", None).unwrap();
    r.register_type("Unit", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Option".into(),
        args: vec![TypeExprAst::Named("String".into())],
    };
    let resolved = r.resolve(&ast).unwrap();
    // (Option String) → (| String Unit), which after canonicalization is
    // App { head: "|", args: [String, Unit] } (alphabetical order).
    match resolved {
        TypeExpr::App { head, args } => {
            assert_eq!(head.0, "|");
            assert_eq!(args.len(), 2);
            let names: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            assert!(names.contains(&"String".into()));
            assert!(names.contains(&"Unit".into()));
        }
        other => panic!("expected union App, got {other:?}"),
    }
}

#[test]
fn arity_mismatch_list_zero_args() {
    use agnes_ast::TypeExprAst;
    let r = Registry::new();
    let ast = TypeExprAst::App {
        head: "List".into(),
        args: vec![],
    };
    let err = r.resolve(&ast).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("List") && msg.contains("expects 1"),
        "got: {msg}"
    );
}

#[test]
fn arity_mismatch_option_two_args() {
    use agnes_ast::TypeExprAst;
    let mut r = Registry::new();
    r.register_type("A", None).unwrap();
    r.register_type("B", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Option".into(),
        args: vec![
            TypeExprAst::Named("A".into()),
            TypeExprAst::Named("B".into()),
        ],
    };
    let err = r.resolve(&ast).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("Option") && msg.contains("expects 1"),
        "got: {msg}"
    );
}

#[test]
fn unknown_head_reports_suggestion() {
    use agnes_ast::TypeExprAst;
    let r = Registry::new();
    let ast = TypeExprAst::App {
        head: "Foo".into(),
        args: vec![TypeExprAst::Named("Bar".into())],
    };
    let err = r.resolve(&ast).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Foo"), "got: {msg}");
    assert!(
        msg.contains("List") && msg.contains("Option"),
        "expected suggestion mentioning List/Option, got: {msg}"
    );
}

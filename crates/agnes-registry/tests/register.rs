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

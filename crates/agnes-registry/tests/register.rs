use agnes_registry::Registry;
use agnes_types::TypeName;

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
    let expr = agnes_types::TypeExpr::Named(TypeName("PDF".into()));
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
        agnes_types::TypeExpr::Union(
            [TypeName("PlainText".into()), TypeName("Markdown".into()), TypeName("HTML".into())]
                .into_iter().collect()
        ),
    ).unwrap();

    // (TextLike | PDF) should resolve to a flat 4-member set.
    r.register_type("PDF", None).unwrap();
    let ast = TypeExprAst::Union(vec![
        TypeExprAst::Named("TextLike".into()),
        TypeExprAst::Named("PDF".into()),
    ]);
    let resolved = r.resolve(&ast).unwrap();
    let set = resolved.as_set();
    assert_eq!(set.len(), 4);
    assert!(set.contains(&TypeName("PlainText".into())));
    assert!(set.contains(&TypeName("PDF".into())));
}

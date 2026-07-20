use agnes_types::{TypeExpr, TypeName, canonicalize_union, type_expr_matches};

#[test]
fn unknown_expected_matches_any_named() {
    let expected = TypeExpr::named("Unknown");
    assert!(type_expr_matches(&TypeExpr::named("PlainText"), &expected));
    assert!(type_expr_matches(&TypeExpr::named("Summary"), &expected));
    assert!(type_expr_matches(&TypeExpr::named("Unit"), &expected));
    // Even Unknown itself.
    assert!(type_expr_matches(&TypeExpr::named("Unknown"), &expected));
}

#[test]
fn unknown_expected_matches_apps() {
    let expected = TypeExpr::named("Unknown");
    // (List PlainText)
    let list_pt = TypeExpr::App {
        head: TypeName("List".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert!(type_expr_matches(&list_pt, &expected));
    // (Finish Summary)
    let finish_summary = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("Summary")],
    };
    assert!(type_expr_matches(&finish_summary, &expected));
    // (| PlainText Markdown)
    let union = canonicalize_union([TypeExpr::named("PlainText"), TypeExpr::named("Markdown")]);
    assert!(type_expr_matches(&union, &expected));
}

#[test]
fn unknown_actual_still_only_matches_unknown_expected() {
    // Wildcard is one-directional: `Unknown` on the ACTUAL side does NOT
    // match every expected. (This preserves the existing behavior of
    // list-literal narrowing at the runtime boundary.)
    let actual = TypeExpr::named("Unknown");
    assert!(type_expr_matches(&actual, &TypeExpr::named("Unknown")));
    assert!(!type_expr_matches(&actual, &TypeExpr::named("PlainText")));
    assert!(!type_expr_matches(&actual, &TypeExpr::named("Summary")));
}

#[test]
fn union_containing_unknown_matches_anything() {
    // (| Unknown Unit) — pathological but confirms unions distribute the
    // wildcard correctly.
    let expected = canonicalize_union([TypeExpr::named("Unknown"), TypeExpr::named("Unit")]);
    assert!(type_expr_matches(&TypeExpr::named("PlainText"), &expected));
    assert!(type_expr_matches(&TypeExpr::named("Summary"), &expected));
}

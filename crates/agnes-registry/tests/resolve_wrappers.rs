use agnes_ast::TypeExprAst;
use agnes_registry::{Registry, RegistryError};
use agnes_types::{TypeExpr, TypeName};

fn ast_named(s: &str) -> TypeExprAst {
    TypeExprAst::Named(s.into())
}

#[test]
fn resolve_finish_single_arg() {
    let mut reg = Registry::new();
    reg.register_type("PlainText", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Finish".into(),
        args: vec![ast_named("PlainText")],
    };
    let got = reg.resolve(&ast).unwrap();
    let expected = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::named("PlainText")],
    };
    assert_eq!(got, expected);
}

#[test]
fn resolve_observation_single_arg() {
    let mut reg = Registry::new();
    reg.register_type("Summary", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Observation".into(),
        args: vec![ast_named("Summary")],
    };
    let got = reg.resolve(&ast).unwrap();
    let expected = TypeExpr::App {
        head: TypeName("Observation".into()),
        args: vec![TypeExpr::named("Summary")],
    };
    assert_eq!(got, expected);
}

#[test]
fn resolve_finish_wrong_arity_rejects() {
    let mut reg = Registry::new();
    reg.register_type("PlainText", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Finish".into(),
        args: vec![ast_named("PlainText"), ast_named("PlainText")],
    };
    let err = reg.resolve(&ast).unwrap_err();
    match err {
        RegistryError::ArityMismatch {
            head,
            expected,
            actual,
        } => {
            assert_eq!(head, "Finish");
            assert_eq!(expected, 1);
            assert_eq!(actual, 2);
        }
        other => panic!("expected ArityMismatch, got {other:?}"),
    }
}

#[test]
fn resolve_observation_zero_args_rejects() {
    let reg = Registry::new();
    let ast = TypeExprAst::App {
        head: "Observation".into(),
        args: vec![],
    };
    let err = reg.resolve(&ast).unwrap_err();
    assert!(matches!(err, RegistryError::ArityMismatch { .. }));
}

#[test]
fn resolve_nested_finish_of_list_of_plaintext() {
    let mut reg = Registry::new();
    reg.register_type("PlainText", None).unwrap();
    let ast = TypeExprAst::App {
        head: "Finish".into(),
        args: vec![TypeExprAst::App {
            head: "List".into(),
            args: vec![ast_named("PlainText")],
        }],
    };
    let got = reg.resolve(&ast).unwrap();
    let expected = TypeExpr::App {
        head: TypeName("Finish".into()),
        args: vec![TypeExpr::App {
            head: TypeName("List".into()),
            args: vec![TypeExpr::named("PlainText")],
        }],
    };
    assert_eq!(got, expected);
}

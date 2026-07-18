use agnes_ast::{Expr, Literal, TopLevel, TypeExprAst};
use agnes_parser::parse;

#[test]
fn parses_a_single_pipe() {
    let src = r#"
        (pipe
          (tool read-file :path "x")
          (tool summarize))
    "#;
    let p = parse(src).expect("parse ok");
    assert!(p.toplevels.is_empty());
    match p.main.expect("has main") {
        Expr::Pipe { steps, .. } => {
            assert_eq!(steps.len(), 2);
            match &steps[0] {
                Expr::Tool { name, args, .. } => {
                    assert_eq!(name, "read-file");
                    assert_eq!(args.len(), 1);
                    assert_eq!(args[0].0, "path");
                    assert!(matches!(&args[0].1,
                        Expr::Literal { lit: Literal::String(s), .. } if s == "x"));
                }
                other => panic!("expected Tool, got {other:?}"),
            }
        }
        other => panic!("expected Pipe, got {other:?}"),
    }
}

#[test]
fn parses_declare_type() {
    let src = r#"(declare type PDF)"#;
    let p = parse(src).expect("parse ok");
    assert_eq!(p.toplevels.len(), 1);
    match &p.toplevels[0] {
        TopLevel::DeclareType { name, .. } => assert_eq!(name, "PDF"),
        other => panic!("expected DeclareType, got {other:?}"),
    }
}

#[test]
fn parses_declare_type_alias() {
    let src = r#"(declare type-alias TextLike (PlainText | Markdown | HTML))"#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTypeAlias { name, expr, .. } => {
            assert_eq!(name, "TextLike");
            match expr {
                TypeExprAst::Union(members) => assert_eq!(members.len(), 3),
                other => panic!("expected Union, got {other:?}"),
            }
        }
        other => panic!("expected DeclareTypeAlias, got {other:?}"),
    }
}

#[test]
fn parses_declare_tool() {
    let src = r#"
        (declare tool ocr
          :requires [(source: (PDF | Image))]
          :provides PlainText)
    "#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTool { name, requires, provides, .. } => {
            assert_eq!(name, "ocr");
            assert_eq!(requires.len(), 1);
            assert_eq!(requires[0].name, "source");
            assert!(matches!(provides, TypeExprAst::Named(s) if s == "PlainText"));
        }
        other => panic!("expected DeclareTool, got {other:?}"),
    }
}

#[test]
fn parses_define_with_body() {
    let src = r#"
        (define greet
          :params [(who: PlainText)]
          :provides PlainText
          (tool llm :prompt "hello" :input who))
    "#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::Define { name, params, .. } => {
            assert_eq!(name, "greet");
            assert_eq!(params.len(), 1);
        }
        other => panic!("expected Define, got {other:?}"),
    }
}

#[test]
fn parses_let_two_forms() {
    let src = r#"
        (pipe
          (tool read-file :path "x")
          (let doc)
          (par
            (let sum (tool summarize doc))
            (let ja  (tool translate :lang "ja"))))
    "#;
    let _ = parse(src).expect("parse ok");
}

#[test]
fn rejects_unclosed_paren() {
    let src = r#"(pipe (tool read-file :path "x")"#;
    assert!(parse(src).is_err());
}

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
    let src = r#"(declare type-alias TextLike (| PlainText Markdown HTML))"#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTypeAlias { name, expr, .. } => {
            assert_eq!(name, "TextLike");
            match expr {
                TypeExprAst::App { head, args } => {
                    assert_eq!(head, "|");
                    assert_eq!(args.len(), 3);
                }
                other => panic!("expected App union, got {other:?}"),
            }
        }
        other => panic!("expected DeclareTypeAlias, got {other:?}"),
    }
}

#[test]
fn parses_prefix_union() {
    let src = r#"(declare type-alias TextLike (| PlainText Markdown HTML))"#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTypeAlias { name, expr, .. } => {
            assert_eq!(name, "TextLike");
            match expr {
                TypeExprAst::App { head, args } => {
                    assert_eq!(head, "|");
                    assert_eq!(args.len(), 3);
                }
                other => panic!("expected App, got {other:?}"),
            }
        }
        other => panic!("expected DeclareTypeAlias, got {other:?}"),
    }
}

#[test]
fn rejects_infix_union() {
    let src = r#"(declare type-alias T (PlainText | Markdown))"#;
    let err = parse(src).expect_err("must reject infix union");
    let msg = format!("{err}");
    assert!(
        msg.contains("union") && msg.contains("prefix"),
        "expected migration hint about prefix form, got: {msg}"
    );
}

#[test]
fn parses_declare_tool_position_based_param() {
    // (source (| PDF Image)) — no trailing colon on the name.
    let src = r#"
        (declare tool ocr
          :requires [(source (| PDF Image))]
          :provides PlainText)
    "#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::DeclareTool { requires, .. } => {
            assert_eq!(requires.len(), 1);
            assert_eq!(requires[0].name, "source");
            // Type is (App { head: "|", args }) with 2 members.
            match &requires[0].ty {
                TypeExprAst::App { head, args } => {
                    assert_eq!(head, "|");
                    assert_eq!(args.len(), 2);
                }
                other => panic!("expected App union, got {other:?}"),
            }
        }
        other => panic!("expected DeclareTool, got {other:?}"),
    }
}

#[test]
fn parses_define_position_based_params() {
    let src = r#"
        (define greet
          :params [(who PlainText) (times Int :default 1)]
          :provides PlainText
          (tool llm :prompt "hello" :input who))
    "#;
    let p = parse(src).expect("parse ok");
    match &p.toplevels[0] {
        TopLevel::Define { params, .. } => {
            assert_eq!(params.len(), 2);
            assert_eq!(params[0].name, "who");
            assert_eq!(params[1].name, "times");
            assert_eq!(params[1].default, Some(agnes_ast::Literal::Int(1)));
        }
        other => panic!("expected Define, got {other:?}"),
    }
}

#[test]
fn rejects_old_colon_suffix_param_syntax() {
    let src = r#"
        (declare tool foo
          :requires [(x: PlainText)]
          :provides PlainText)
    "#;
    let err = parse(src).expect_err("must reject legacy param syntax");
    let msg = format!("{err}");
    assert!(
        msg.contains("param name") && msg.contains("no longer ends with"),
        "expected migration hint, got: {msg}"
    );
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

#[test]
fn parses_source_with_non_ascii_content() {
    // Ensure non-ASCII bytes in string literals survive preprocessing.
    let src = r#"(tool llm :prompt "你好 world" :input "test")"#;
    let p = agnes_parser::parse(src).expect("parse ok");
    let main = p.main.expect("has main");
    // Verify the string literal came through intact.
    let matches = format!("{:?}", main).contains("你好 world");
    assert!(
        matches,
        "expected non-ASCII string preserved, got: {main:?}"
    );
}

#[test]
fn parses_list_form() {
    let src = r#"(list "a" "b" "c")"#;
    let p = parse(src).expect("parse ok");
    match p.main.expect("has main") {
        Expr::List { items, .. } => {
            assert_eq!(items.len(), 3);
            assert!(matches!(&items[0], Expr::Literal { lit: Literal::String(s), .. } if s == "a"));
        }
        other => panic!("expected Expr::List, got {other:?}"),
    }
}

#[test]
fn parses_bracket_list() {
    let src = r#"["a" "b"]"#;
    let p = parse(src).expect("parse ok");
    match p.main.expect("has main") {
        Expr::List { items, .. } => assert_eq!(items.len(), 2),
        other => panic!("expected Expr::List, got {other:?}"),
    }
}

#[test]
fn parses_empty_bracket_list() {
    let src = r#"[]"#;
    let p = parse(src).expect("parse ok");
    match p.main.expect("has main") {
        Expr::List { items, .. } => assert!(items.is_empty()),
        other => panic!("expected Expr::List, got {other:?}"),
    }
}

#[test]
fn rejects_comma_in_bracket_list() {
    let src = r#"["a", "b"]"#;
    let err = parse(src).expect_err("must reject commas");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("comma") || msg.to_lowercase().contains("whitespace"),
        "expected comma hint, got: {msg}"
    );
}

#[test]
fn parses_nested_bracket_list() {
    let src = r#"[["a"] ["b" "c"]]"#;
    let p = parse(src).expect("parse ok");
    match p.main.expect("has main") {
        Expr::List { items, .. } => {
            assert_eq!(items.len(), 2);
            assert!(matches!(&items[0], Expr::List { items, .. } if items.len() == 1));
            assert!(matches!(&items[1], Expr::List { items, .. } if items.len() == 2));
        }
        other => panic!("expected outer Expr::List, got {other:?}"),
    }
}

#[test]
fn positional_tool_arg_uses_positional_vec() {
    let src = r#"(tool summarize doc)"#;
    let p = agnes_parser::parse(src).unwrap();
    match p.main.unwrap() {
        agnes_ast::Expr::Tool {
            name,
            positional,
            args,
            ..
        } => {
            assert_eq!(name, "summarize");
            assert_eq!(positional.len(), 1);
            assert!(args.is_empty(), "no synthetic kwargs");
            assert!(matches!(&positional[0], agnes_ast::Expr::Var { name, .. } if name == "doc"));
        }
        other => panic!("expected Tool, got {other:?}"),
    }
}

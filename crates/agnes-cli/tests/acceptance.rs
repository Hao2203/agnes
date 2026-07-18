//! End-to-end acceptance tests locking in spec §VII behavior.
//!
//! Three positive workflows (full-demo shape with `define` + `pipe` + `par` +
//! `let` + `llm`; `join-lines` over a list literal of tool calls; a compound
//! tool with an `(Option String)` param) plus ten negative cases that
//! exercise each error surface: `(List ...)` and `(Option ...)` arity
//! mismatches, unknown constructor heads, infix-union rejection,
//! comma-in-list rejection, mixed-list element-type rejection, flow
//! mismatch, recursive define, unknown type name, and name conflict.

use agnes_builtins::{native_dispatch, register_builtins};
use agnes_checker::check;
use agnes_compiler::compile;
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_runtime::execute;

async fn run(src: &str) -> Result<String, String> {
    let mut reg = Registry::new();
    register_builtins(&mut reg).map_err(|e| format!("{e}"))?;
    let program = parse(src).map_err(|e| format!("{e}"))?;
    reg.load(&program).map_err(|e| format!("{e}"))?;
    check(&program, &reg).map_err(|e| format!("{e}"))?;
    let dag = compile(&program, &reg).map_err(|e| format!("{e}"))?;
    let dispatch = native_dispatch();
    let value = execute(&dag, &reg, &dispatch)
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(value.data.to_string())
}

async fn seed_readme() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "agnes-acceptance-readme-{}-{n}.md",
        std::process::id()
    ));
    tokio::fs::write(&path, "hello world\n").await.unwrap();
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn positive_full_demo_runs() {
    let readme = seed_readme().await;
    let src = format!(
        r#"
(define read-and-translate
  :params  [(path Path) (target String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(pipe
  (let ja (tool read-and-translate :path "{readme}" :target "ja"))
  (tool join-lines :lines [ja ja]))
"#
    );
    let out = run(&src).await.expect("full-demo workflow must succeed");
    assert!(
        out.contains("[TRANSLATED"),
        "expected translated content in joined output, got: {out}"
    );
    let _ = tokio::fs::remove_file(&readme).await;
}

#[tokio::test]
async fn positive_join_lines_with_list_literal() {
    let readme = seed_readme().await;
    let src = format!(
        r#"
(tool join-lines :lines [(tool read-file :path "{readme}")
                          (tool read-file :path "{readme}")])
"#
    );
    let out = run(&src).await.expect("join-lines must succeed");
    // The mock implementation of join-lines concatenates array elements with '\n'.
    assert!(out.contains("hello world"), "got: {out}");
    let _ = tokio::fs::remove_file(&readme).await;
}

#[tokio::test]
async fn positive_option_string_declares_param() {
    let src = r#"
        (define maybe-greet
          :params [(name (Option String))]
          :provides PlainText
          (tool llm :prompt "greet" :input "hi"))
        (tool maybe-greet :name "world")
    "#;
    let out = run(src).await.expect("Option String param must work");
    assert!(out.contains("[LLM"), "got: {out}");
}

#[tokio::test]
async fn negative_list_arity_mismatch() {
    let src = r#"(declare tool bad :requires [(x (List))] :provides PlainText)"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("List") && msg.contains("expects 1"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn negative_option_arity_mismatch() {
    let src = r#"(declare tool bad :requires [(x (Option A B))] :provides PlainText)"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("Option") && msg.contains("expects 1"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn negative_unknown_head_suggests_builtins() {
    let src = r#"(declare tool bad :requires [(x (Foo Bar))] :provides PlainText)"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(msg.contains("Foo"), "got: {msg}");
    assert!(msg.contains("List") || msg.contains("Option"), "got: {msg}");
}

#[tokio::test]
async fn negative_infix_union_rejected() {
    let src = r#"(declare type-alias T (A | B))"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("union") && msg.contains("prefix"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn negative_comma_in_bracket_list() {
    let src = r#"(tool llm :prompt "x" :input ["a", "b"])"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("comma") || msg.to_lowercase().contains("whitespace"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn negative_mixed_list_where_string_list_expected() {
    // join-lines accepts (List (| PlainText Markdown)) — passing (List Int)
    // via a mixed literal fails.
    let src = r#"(tool join-lines :lines ["a" 1])"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(msg.contains("List"), "got: {msg}");
}

#[tokio::test]
async fn negative_flow_type_mismatch() {
    // read-file provides PlainText; ocr requires ScannedImage → checker rejects.
    let src = r#"(pipe (tool read-file :path "x.md") (tool ocr))"#;
    let err = run(src).await.expect_err("must fail type check");
    assert!(err.contains("Type error"), "missing 'Type error': {err}");
    assert!(err.contains("ocr"), "missing 'ocr': {err}");
    assert!(
        err.contains("Fix suggestion"),
        "missing 'Fix suggestion': {err}"
    );
}

#[tokio::test]
async fn negative_recursive_define() {
    let src = r#"(define loopy :params [] :provides Unit (tool loopy))"#;
    let err = run(src).await.expect_err("must fail compile");
    assert!(
        err.contains("Recursive define"),
        "missing 'Recursive define': {err}"
    );
    assert!(err.contains("loopy"), "missing 'loopy': {err}");
}

#[tokio::test]
async fn negative_unknown_type() {
    let src = r#"(declare tool weird :requires [(x MysteryType)] :provides PlainText)"#;
    let err = run(src).await.expect_err("must fail registry load");
    assert!(
        err.contains("Unknown name"),
        "missing 'Unknown name': {err}"
    );
    assert!(err.contains("MysteryType"), "missing 'MysteryType': {err}");
    assert!(
        err.contains("declare type"),
        "missing 'declare type': {err}"
    );
}

#[tokio::test]
async fn negative_name_conflict() {
    // PlainText already registered as a builtin type.
    let src = r#"(declare type PlainText)"#;
    let err = run(src).await.expect_err("must fail registry load");
    assert!(
        err.contains("Name conflict"),
        "missing 'Name conflict': {err}"
    );
    assert!(err.contains("PlainText"), "missing 'PlainText': {err}");
}

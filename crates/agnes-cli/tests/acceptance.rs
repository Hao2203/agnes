//! End-to-end acceptance tests locking in spec §VII behavior.
//!
//! One positive workflow (full-demo shape with `define` + `pipe` + `par` +
//! `let` + `llm`) plus four negative cases that exercise each error surface.

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
    let path =
        std::env::temp_dir().join(format!("agnes-acceptance-readme-{}.md", std::process::id()));
    tokio::fs::write(&path, "hello world\n").await.unwrap();
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn positive_full_demo_runs() {
    let readme = seed_readme().await;
    // Mirrors examples/full-demo.agnes: feed `ja` (PlainText) into `llm :input`
    // (PlainText) rather than `sum` (Summary), which would be a type error.
    let src = format!(
        r#"
(define read-and-translate
  :params  [(path Path) (target String)]
  :provides PlainText
  (pipe
    (tool read-file :path path)
    (tool translate :lang target)))

(pipe
  (let src (tool read-file :path "{readme}"))
  (par
    (let sum (tool summarize :input src))
    (let ja  (tool read-and-translate :path "{readme}" :target "ja")))
  (tool llm :prompt "combine" :input ja))
"#
    );
    let out = run(&src).await.expect("full-demo workflow must succeed");
    assert!(
        out.contains("[LLM prompt=combine"),
        "expected LLM output marker, got: {out}"
    );
    let _ = tokio::fs::remove_file(&readme).await;
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

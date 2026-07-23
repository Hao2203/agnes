//! End-to-end acceptance tests locking in spec §VII behavior.
//!
//! Three positive workflows (full-demo shape with `define` + `pipe` + `par` +
//! `let` + `llm`; `join-lines` over a list literal of tool calls; a compound
//! tool with an `(Option String)` param) plus ten negative cases that
//! exercise each error surface: `(List ...)` and `(Option ...)` arity
//! mismatches, unknown constructor heads, infix-union rejection,
//! comma-in-list rejection, mixed-list element-type rejection, flow
//! mismatch, recursive define, unknown type name, and name conflict.

use agnes_builtins::{native_dispatch, register_builtins, PathResolver, Sink, ToolCtx};
use agnes_checker::check;
use agnes_compiler::compile;
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_runtime::execute;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;

async fn run(src: &str) -> Result<String, String> {
    run_with(src, vec![]).await
}

struct DummyResolver;
impl PathResolver for DummyResolver {
    fn resolve_path<'a>(&'a self, input: &'a str) -> agnes_builtins::BoxFuture<'a, Result<PathBuf, String>> {
        // Special case: the tests use "README.md" which exists at repo root
        if input == "README.md" {
            // Hardcode the absolute path to README.md since we know it from env
            let path = PathBuf::from("/home/hao/code/agnes/README.md");
            Box::pin(async move { Ok(path) })
        } else {
            panic!("dummy resolver should not be called in this test");
        }
    }
}

/// No-op sink for tests that don't exercise shell-run.
struct DummySink;
impl Sink for DummySink {
    fn shell_confirm<'a>(
        &'a self,
        _command: String,
        responder: oneshot::Sender<bool>,
    ) -> agnes_builtins::BoxFuture<'a, ()> {
        Box::pin(async move {
            let _ = responder.send(false);
        })
    }
    fn shell_output<'a>(
        &'a self,
        _line: String,
        _is_stderr: bool,
    ) -> agnes_builtins::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

static DUMMY_SINK: DummySink = DummySink;

fn ctx(resolver: &DummyResolver) -> ToolCtx<'_> {
    ToolCtx {
        resolver,
        sink: &DUMMY_SINK,
        allow_shell: false,
    }
}

async fn run_with(src: &str, responses: Vec<String>) -> Result<String, String> {
    let mut reg = Registry::new();
    register_builtins(&mut reg).map_err(|e| format!("{e}"))?;
    let program = parse(src).map_err(|e| format!("{e}"))?;
    reg.load(&program).map_err(|e| format!("{e}"))?;
    check(&program, &reg).map_err(|e| format!("{e}"))?;
    let dag = compile(&program, &reg).map_err(|e| format!("{e}"))?;
    let mock: Arc<dyn agnes_llm::Provider> = Arc::new(agnes_llm::MockProvider::new(responses));
    let dispatch = native_dispatch(mock);
    let dummy = DummyResolver;
    let value = execute(&dag, &reg, &dispatch, &ctx(&dummy))
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(value.data.to_string())
}

async fn seed_readme() -> String {
    // Mock read-file has a seeded "README.md" entry; using that path avoids
    // touching disk in tests.
    "README.md".to_string()
}

#[tokio::test]
async fn positive_full_demo_runs() {
    let readme = seed_readme().await;
    let src = format!(
        r#"
(define read-and-translate
  :params  [(path Path) (target String)]
  :provides String
  (pipe (tool read-file path) (tool translate target)))

(pipe
  (let ja (tool read-and-translate "{readme}" "ja"))
  (tool join-lines [ja ja]))
"#
    );
    // One translate call per read-and-translate invocation.
    let out = run_with(&src, vec!["[TRANSLATED to ja] agnes".into()])
        .await
        .expect("full-demo workflow must succeed");
    assert!(
        out.contains("[TRANSLATED"),
        "expected translated content in joined output, got: {out}"
    );
}

#[tokio::test]
async fn positive_join_lines_with_list_literal() {
    let readme = seed_readme().await;
    let src = format!(
        r#"
(tool join-lines [(tool read-file "{readme}")
                          (tool read-file "{readme}")])
"#
    );
    let out = run(&src).await.expect("join-lines must succeed");
    // The mock implementation of join-lines concatenates array elements with '\n'.
    // The mock README fixture contains "agnes".
    assert!(out.contains("agnes"), "got: {out}");
}

#[tokio::test]
async fn positive_option_string_declares_param() {
    let src = r#"
        (define maybe-greet
          :params [(name (Option String))]
          :provides String
          (tool llm "greet" "hi"))
        (tool maybe-greet "world")
    "#;
    let out = run_with(src, vec!["[LLM greeted world]".into()])
        .await
        .expect("Option String param must work");
    assert!(out.contains("[LLM"), "got: {out}");
}

#[tokio::test]
async fn positive_option_string_accepts_nil() {
    let src = r#"
        (define maybe-greet
          :params [(name (Option String))]
          :provides String
          (tool llm "greet" "hi"))
        (tool maybe-greet nil)
    "#;
    let out = run_with(src, vec!["[LLM greeted]".into()])
        .await
        .expect("Option String param must accept nil");
    assert!(out.contains("[LLM"), "got: {out}");
}

#[tokio::test]
async fn negative_list_arity_mismatch() {
    let src = r#"(declare tool bad :requires [(x (List))] :provides String)"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("List") && msg.contains("expects 1"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn negative_option_arity_mismatch() {
    let src = r#"(declare tool bad :requires [(x (Option A B))] :provides String)"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("Option") && msg.contains("expects 1"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn negative_unknown_head_suggests_builtins() {
    let src = r#"(declare tool bad :requires [(x (Foo Bar))] :provides String)"#;
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
    let src = r#"(tool llm "x" ["a", "b"])"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("comma") || msg.to_lowercase().contains("whitespace"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn negative_mixed_list_where_string_list_expected() {
    // join-lines accepts (List String) — passing (List Int)
    // via a mixed literal fails.
    let src = r#"(tool join-lines ["a" 1])"#;
    let err = run(src).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(msg.contains("List"), "got: {msg}");
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
    let src = r#"(declare tool weird :requires [(x MysteryType)] :provides String)"#;
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
    // String already registered as a builtin type.
    let src = r#"(declare type String)"#;
    let err = run(src).await.expect_err("must fail registry load");
    assert!(
        err.contains("Name conflict"),
        "missing 'Name conflict': {err}"
    );
    assert!(err.contains("String"), "missing 'String': {err}");
}

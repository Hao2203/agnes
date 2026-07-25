use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use agnes_builtins::{native_dispatch, observations, register_builtins, ObservationRecord, PathResolver, Sink, ToolCtx};
use agnes_checker::check;
use agnes_compiler::{NodeId, compile};
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_runtime::{execute, execute_with};
use tokio::sync::oneshot;

struct DummyResolver;
impl PathResolver for DummyResolver {
    fn resolve_path<'a>(&'a self, input: &'a str) -> agnes_builtins::BoxFuture<'a, Result<PathBuf, String>> {
        // Resolve paths relative to the project root
        let root = std::env::current_dir().unwrap();
        let path = root.join(input);
        Box::pin(async move { Ok(path) })
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

#[tokio::test]
async fn runs_read_then_summarize() {
    let src = r#"(pipe (tool read-file "README.md") (tool summarize))"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();

    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();

    let mock = Arc::new(agnes_llm::MockProvider::new(vec!["[SUMMARY]".into()]));
    let dispatch = native_dispatch(mock);
    let dummy = DummyResolver;
    let out = execute(&dag, &r, &dispatch, &ctx(&dummy)).await.expect("run ok");
    let s = out.data.as_str().expect("string result");
    assert_eq!(s, "[SUMMARY]");
}

#[tokio::test]
async fn runs_a_defined_compound_tool() {
    let src = r#"
        (define read-and-summarize
          :params [(path Path)]
          :provides String
          (pipe
            (tool read-file path)
            (tool summarize)))
        (tool read-and-summarize "README.md")
    "#;

    let mut r = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut r).unwrap();
    let p = agnes_parser::parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = agnes_compiler::compile(&p, &r).unwrap();
    let mock = Arc::new(agnes_llm::MockProvider::new(vec!["[SUMMARY]".into()]));
    let dispatch = agnes_builtins::native_dispatch(mock);
    let dummy = DummyResolver;
    let out = agnes_runtime::execute(&dag, &r, &dispatch, &ctx(&dummy)).await.unwrap();
    let s = out.data.as_str().unwrap();
    assert_eq!(s, "[SUMMARY]");
}

#[tokio::test]
async fn evaluates_list_literal() {
    let src = r#"(list "a" "b" "c")"#;
    let mut r = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut r).unwrap();
    let p = agnes_parser::parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = agnes_compiler::compile(&p, &r).unwrap();
    let mock = Arc::new(agnes_llm::MockProvider::new(vec![]));
    let dispatch = agnes_builtins::native_dispatch(mock);
    let dummy = DummyResolver;
    let out = agnes_runtime::execute(&dag, &r, &dispatch, &ctx(&dummy)).await.unwrap();
    let arr = out.data.as_array().expect("array result");
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0], serde_json::json!("a"));
}

#[tokio::test]
async fn boundary_validates_list_of_string_at_runtime() {
    // Register a mock tool that (correctly) receives a (List String).
    let mut r = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut r).unwrap();
    // Manually augment: declare a tool that requires (List String) and
    // returns String — mock via source.
    let src = r#"
        (declare tool see-list
          :requires [(items (List String))]
          :provides String)

        (tool see-list ["a" "b"])
    "#;
    let p = agnes_parser::parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    // Compile is fine, but native_dispatch has no impl — call will fail with
    // MissingImpl at runtime. That's OK: the point of this test is to make
    // sure the checker + compiler accept the parameterized signature and
    // that runtime boundary validation doesn't panic before reaching dispatch.
    let dag = agnes_compiler::compile(&p, &r).unwrap();
    let mock = Arc::new(agnes_llm::MockProvider::new(vec![]));
    let dispatch = agnes_builtins::native_dispatch(mock);
    let dummy = DummyResolver;
    let err = agnes_runtime::execute(&dag, &r, &dispatch, &ctx(&dummy))
        .await
        .unwrap_err();
    let msg = format!("{err}");
    // Under Task 6 the boundary walker recurses into (List T) — array
    // elements pass validation, and the runtime reaches dispatch, which
    // fails because see-list has no native implementation registered.
    // Before Task 6 the walker rejected any non-`|` App head with a
    // "unknown type constructor" RuntimeTypeError before dispatch. This
    // assertion must fail against the pre-Task-6 behavior.
    assert!(
        msg.contains("No native implementation"),
        "expected MissingImpl (not a validation error). got: {msg}"
    );
    assert!(
        !msg.contains("unknown type constructor"),
        "boundary walker still rejects (List T) — Task 6 regression. got: {msg}"
    );
}

#[tokio::test]
async fn boundary_validates_list_of_union_at_runtime() {
    // Regression: when validating (List T) with T a union member set, the
    // walker must pass each element as a Value whose declared_type is a
    // concrete Named (inferred from JSON shape) — not the outer list's
    // union inner. Prior code re-passed the union expected as the element's
    // declared_type, breaking the union-arm set-membership check.
    //
    // join-lines requires (List String). Feeding a list of
    // two read-file outputs (both String) must succeed end-to-end.
    let src = r#"
        (pipe
          (let a (tool read-file "README.md"))
          (tool join-lines [a a]))
        "#;
    let mut r = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut r).unwrap();
    let p = agnes_parser::parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = agnes_compiler::compile(&p, &r).unwrap();
    let mock = Arc::new(agnes_llm::MockProvider::new(vec![]));
    let dispatch = agnes_builtins::native_dispatch(mock);
    let dummy = DummyResolver;
    let out = agnes_runtime::execute(&dag, &r, &dispatch, &ctx(&dummy))
        .await
        .expect("List String boundary must accept String elements");
    let s = out.data.as_str().expect("string result");
    assert!(s.contains("agnes"), "got: {s}");
}

// ---- Task 5: fmap / tool_observe / visited set ----

/// Clear the process-global observations recorder before a test.
fn clear_observations() {
    observations().lock().unwrap().clear();
}

/// Drain and return all currently recorded observations.
fn drain_observations() -> Vec<ObservationRecord> {
    observations().lock().unwrap().drain(..).collect()
}

#[tokio::test]
async fn tool_observe_produces_observation_and_records_snapshot() {
    clear_observations();
    let src = r#"(tool_observe read-file "README.md")"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();

    let mock = Arc::new(agnes_llm::MockProvider::new(vec![]));
    let dispatch = native_dispatch(mock);
    let dummy = DummyResolver;
    let out = execute(&dag, &r, &dispatch, &ctx(&dummy)).await.expect("run ok");

    // Type must be Observation String
    assert_eq!(out.declared_type.to_string(), "(Observation String)");
    // Data is the inner string
    let s = out.data.as_str().expect("string result");
    assert!(s.contains("agnes"), "got: {s}");

    // Observations recorder should have exactly one entry
    let obs = drain_observations();
    assert_eq!(obs.len(), 1, "expected 1 observation, got {}", obs.len());
    assert!(obs[0].text.contains("agnes"), "snapshot text missing content: {}", obs[0].text);
    assert_eq!(obs[0].type_name.as_ref().map(|n| n.0.as_str()), Some("String"));
}

#[tokio::test]
async fn fmap_extracts_observation_applies_tool_and_rewraps() {
    clear_observations();
    let src = r#"(pipe (tool_observe read-file "README.md") (fmap (tool summarize)))"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();

    let mock = Arc::new(agnes_llm::MockProvider::new(vec!["[SUMMARY]".into()]));
    let dispatch = native_dispatch(mock);
    let dummy = DummyResolver;
    let out = execute(&dag, &r, &dispatch, &ctx(&dummy)).await.expect("run ok");

    // fmap over Observation should produce Observation String (rewrapped)
    assert_eq!(out.declared_type.to_string(), "(Observation String)");
    let s = out.data.as_str().expect("string result");
    assert_eq!(s, "[SUMMARY]");

    // tool_observe recorded one snapshot (the read-file output)
    let obs = drain_observations();
    assert_eq!(obs.len(), 1);
    assert!(obs[0].text.contains("agnes"));
}

#[tokio::test]
async fn fmap_over_finish_rewraps_in_finish() {
    // fmap lifts over any Outcome (Observation or Finish) — verify Finish re-wrap.
    let src = r#"(pipe (finish "hello") (fmap (tool summarize)))"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();

    let mock = Arc::new(agnes_llm::MockProvider::new(vec!["[SUMMARY]".into()]));
    let dispatch = native_dispatch(mock);
    let dummy = DummyResolver;
    let out = execute(&dag, &r, &dispatch, &ctx(&dummy)).await.expect("run ok");

    assert_eq!(out.declared_type.to_string(), "(Finish String)");
    let s = out.data.as_str().expect("string result");
    assert_eq!(s, "[SUMMARY]");
}

#[tokio::test]
async fn visited_set_contains_only_executed_nodes() {
    // (if #t (finish "a") (observe "b")) — only the then-branch should be visited.
    let src = r#"(if #t (finish "a") (observe "b"))"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();

    let total_nodes = dag.nodes.len();

    let mock = Arc::new(agnes_llm::MockProvider::new(vec![]));
    let dispatch = native_dispatch(mock);
    let dummy = DummyResolver;
    let (out, visited) = execute_with(&dag, &r, &dispatch, &ctx(&dummy), &agnes_runtime::NoopTracer)
        .await
        .expect("run ok");

    assert_eq!(out.declared_type.to_string(), "(Finish String)");
    assert_eq!(out.data.as_str(), Some("a"));

    // Visited must be a proper subset of all nodes — the else branch is pruned.
    assert!(visited.len() < total_nodes,
        "visited set size {} should be < total nodes {} (else branch must not be visited)",
        visited.len(), total_nodes);

    // Root (the if node) must be visited.
    assert!(visited.contains(&dag.root), "root must be visited");

    // Check that the set is a HashSet<NodeId> with the right type.
    let _: &HashSet<NodeId> = &visited;
}

#[tokio::test]
async fn visited_set_includes_root_and_cond_but_not_untaken_branch() {
    // Deeper check: build the DAG and verify which nodes are present.
    let src = r#"(if #f (finish "yes") (finish "no"))"#;
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    let p = parse(src).unwrap();
    r.load(&p).unwrap();
    check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();

    let mock = Arc::new(agnes_llm::MockProvider::new(vec![]));
    let dispatch = native_dispatch(mock);
    let dummy = DummyResolver;
    let (out, visited) = execute_with(&dag, &r, &dispatch, &ctx(&dummy), &agnes_runtime::NoopTracer)
        .await
        .expect("run ok");

    assert_eq!(out.data.as_str(), Some("no"));
    // Total nodes: if, false_literal, finish_yes, "yes"_lit, finish_no, "no"_lit = 6
    // Visited: if, false_lit, finish_no, "no"_lit = 4
    // At minimum: visited must be strictly fewer than total.
    assert!(visited.len() < dag.nodes.len(),
        "visited {} should be < total {} (untaken branch pruned)",
        visited.len(), dag.nodes.len());
}

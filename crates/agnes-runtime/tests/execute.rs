use agnes_builtins::{native_dispatch, register_builtins, PathResolver};
use agnes_checker::check;
use agnes_compiler::compile;
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_runtime::execute;
use std::path::PathBuf;
use std::sync::Arc;

struct DummyResolver;
impl PathResolver for DummyResolver {
    fn resolve_path<'a>(&'a self, input: &'a str) -> agnes_builtins::BoxFuture<'a, Result<PathBuf, String>> {
        // Resolve paths relative to the project root
        let root = std::env::current_dir().unwrap();
        let path = root.join(input);
        Box::pin(async move { Ok(path) })
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
    let out = execute(&dag, &r, &dispatch, &dummy).await.expect("run ok");
    let s = out.data.as_str().expect("string result");
    assert_eq!(s, "[SUMMARY]");
}

#[tokio::test]
async fn runs_a_defined_compound_tool() {
    let src = r#"
        (define read-and-summarize
          :params [(path Path)]
          :provides Summary
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
    let out = agnes_runtime::execute(&dag, &r, &dispatch, &dummy).await.unwrap();
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
    let out = agnes_runtime::execute(&dag, &r, &dispatch, &dummy).await.unwrap();
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
    // returns PlainText — mock via source.
    let src = r#"
        (declare tool see-list
          :requires [(items (List String))]
          :provides PlainText)

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
    let err = agnes_runtime::execute(&dag, &r, &dispatch, &dummy)
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
    // join-lines requires (List (| PlainText Markdown)). Feeding a list of
    // two read-file outputs (both PlainText) must succeed end-to-end.
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
    let out = agnes_runtime::execute(&dag, &r, &dispatch, &dummy)
        .await
        .expect("List (| PlainText Markdown) boundary must accept PlainText elements");
    let s = out.data.as_str().expect("string result");
    assert!(s.contains("agnes"), "got: {s}");
}

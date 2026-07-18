use agnes_builtins::{native_dispatch, register_builtins};
use agnes_checker::check;
use agnes_compiler::compile;
use agnes_parser::parse;
use agnes_registry::Registry;
use agnes_runtime::execute;

#[tokio::test]
async fn runs_read_then_summarize() {
    let tmp = tempfile_path();
    tokio::fs::write(&tmp, "hello world").await.unwrap();

    let src = format!(r#"(pipe (tool read-file :path "{tmp}") (tool summarize))"#);
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();

    let p = parse(&src).unwrap();
    r.load(&p).unwrap();
    check(&p, &r).unwrap();
    let dag = compile(&p, &r).unwrap();
    let dispatch = native_dispatch();
    let out = execute(&dag, &r, &dispatch).await.expect("run ok");
    let s = out.data.as_str().expect("string result");
    assert!(s.starts_with("[SUMMARY of"), "got: {s}");
    let _ = tokio::fs::remove_file(&tmp).await;
}

#[tokio::test]
async fn runs_a_defined_compound_tool() {
    let tmp = std::env::temp_dir().join(format!("agnes-define-test-{}.txt", std::process::id()));
    tokio::fs::write(&tmp, "content").await.unwrap();
    let src = format!(
        r#"
        (define read-and-summarize
          :params [(path Path)]
          :provides Summary
          (pipe
            (tool read-file :path path)
            (tool summarize)))
        (tool read-and-summarize :path "{}")
    "#,
        tmp.display()
    );

    let mut r = agnes_registry::Registry::new();
    agnes_builtins::register_builtins(&mut r).unwrap();
    let p = agnes_parser::parse(&src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    let dag = agnes_compiler::compile(&p, &r).unwrap();
    let dispatch = agnes_builtins::native_dispatch();
    let out = agnes_runtime::execute(&dag, &r, &dispatch).await.unwrap();
    let s = out.data.as_str().unwrap();
    assert!(s.starts_with("[SUMMARY of"), "got: {s}");
    let _ = tokio::fs::remove_file(&tmp).await;
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
    let dispatch = agnes_builtins::native_dispatch();
    let out = agnes_runtime::execute(&dag, &r, &dispatch).await.unwrap();
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

        (tool see-list :items ["a" "b"])
    "#;
    let p = agnes_parser::parse(src).unwrap();
    r.load(&p).unwrap();
    agnes_checker::check(&p, &r).unwrap();
    // Compile is fine, but native_dispatch has no impl — call will fail with
    // MissingImpl at runtime. That's OK: the point of this test is to make
    // sure the checker + compiler accept the parameterized signature and
    // that runtime boundary validation doesn't panic before reaching dispatch.
    let dag = agnes_compiler::compile(&p, &r).unwrap();
    let dispatch = agnes_builtins::native_dispatch();
    let err = agnes_runtime::execute(&dag, &r, &dispatch)
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

fn tempfile_path() -> String {
    let dir = std::env::temp_dir();
    let stamp = std::process::id();
    dir.join(format!("agnes-test-{stamp}.txt"))
        .to_string_lossy()
        .into_owned()
}

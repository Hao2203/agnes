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

fn tempfile_path() -> String {
    let dir = std::env::temp_dir();
    let stamp = std::process::id();
    dir.join(format!("agnes-test-{stamp}.txt"))
        .to_string_lossy()
        .into_owned()
}

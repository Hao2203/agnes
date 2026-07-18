use agnes_builtins::{native_dispatch, register_builtins};
use agnes_registry::Registry;

#[test]
fn registers_all_builtins() {
    let mut r = Registry::new();
    register_builtins(&mut r).expect("builtins load");
    assert!(r.tool_signature("read-file").is_some());
    assert!(r.tool_signature("write-file").is_some());
    assert!(r.tool_signature("summarize").is_some());
    assert!(r.tool_signature("translate").is_some());
    assert!(r.tool_signature("ocr").is_some());
    assert!(r.tool_signature("llm").is_some());
}

#[test]
fn native_dispatch_has_all_impls() {
    let d = native_dispatch();
    for name in [
        "read-file",
        "write-file",
        "summarize",
        "translate",
        "ocr",
        "llm",
    ] {
        assert!(d.contains_key(name), "missing impl for {name}");
    }
}

#[test]
fn join_lines_registered() {
    let mut r = Registry::new();
    register_builtins(&mut r).expect("builtins load");
    assert!(r.tool_signature("join-lines").is_some());
}

#[test]
fn native_dispatch_has_join_lines() {
    let d = native_dispatch();
    assert!(d.contains_key("join-lines"));
}

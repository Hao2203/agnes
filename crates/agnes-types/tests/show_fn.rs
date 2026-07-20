use agnes_types::ShowFn;
use serde_json::json;

fn my_show(v: &serde_json::Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

#[test]
fn show_fn_alias_accepts_a_matching_fn() {
    let f: ShowFn = my_show;
    let out = f(&json!("hello"));
    assert_eq!(out, "hello");
}

#[test]
fn show_fn_returns_owned_string_even_for_empty() {
    fn empty(_v: &serde_json::Value) -> String {
        String::new()
    }
    let f: ShowFn = empty;
    assert_eq!(f(&json!(null)), "");
}

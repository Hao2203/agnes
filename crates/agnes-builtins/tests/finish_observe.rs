// After the finish/observe → special-form refactor, these constructs are
// no longer registered as tools. What remains testable in agnes-builtins:
//   * The registry still knows about the wrapper *type* names `Finish` and
//     `Observation` (so `(declare tool ... provides (Finish T))` and
//     `show_value` work).
//   * The tool dispatch table no longer contains `finish` or `observe`.
//   * The tool registry no longer lists them under `tool_signature`.
// The full end-to-end wrapping behavior is covered by
// `agnes-session/tests/session_end_to_end.rs`.

use agnes_builtins::{native_dispatch, register_builtins};
use agnes_llm::MockProvider;
use agnes_registry::Registry;
use std::sync::Arc;

fn dispatch() -> std::collections::HashMap<String, agnes_builtins::ToolImpl> {
    let mock = Arc::new(MockProvider::new(vec![]));
    native_dispatch(mock)
}

fn reg() -> Registry {
    let mut r = Registry::new();
    register_builtins(&mut r).unwrap();
    r
}

#[test]
fn finish_is_not_a_registered_tool_after_refactor() {
    let r = reg();
    assert!(
        r.tool_signature("finish").is_none(),
        "finish must no longer be a tool; it's a special form (Expr::Finish)"
    );
}

#[test]
fn observe_is_not_a_registered_tool_after_refactor() {
    let r = reg();
    assert!(
        r.tool_signature("observe").is_none(),
        "observe must no longer be a tool; it's a special form (Expr::Observe)"
    );
}

#[test]
fn finish_is_not_in_the_native_dispatch_table() {
    let d = dispatch();
    assert!(
        d.get("finish").is_none(),
        "native_dispatch must not carry a finish entry any more"
    );
}

#[test]
fn observe_is_not_in_the_native_dispatch_table() {
    let d = dispatch();
    assert!(d.get("observe").is_none(), "same for observe");
}

#[test]
fn finish_and_observation_wrapper_types_stay_registered() {
    // Wrapper types must remain in the registry so `declared_type`s of the
    // form `(Finish T)` / `(Observation T)` resolve, and so `show_value`
    // can strip the wrapper layer.
    let mut r2 = Registry::new();
    register_builtins(&mut r2).unwrap();
    let err = r2.register_type("Finish", None).unwrap_err();
    match err {
        agnes_registry::RegistryError::NameConflict { name, .. } => {
            assert_eq!(name, "Finish");
        }
        other => panic!("expected NameConflict for Finish, got {other:?}"),
    }
    let err = r2.register_type("Observation", None).unwrap_err();
    match err {
        agnes_registry::RegistryError::NameConflict { name, .. } => {
            assert_eq!(name, "Observation");
        }
        other => panic!("expected NameConflict for Observation, got {other:?}"),
    }
}

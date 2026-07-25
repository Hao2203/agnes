use agnes_builtins::{observations, ObservationRecord};
use agnes_types::TypeName;

/// Serialize tests that share the process-global observations() recorder.
fn test_lock() -> &'static std::sync::Mutex<()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
}

fn drain() {
    observations().lock().unwrap().clear();
}

#[test]
fn observations_initially_empty() {
    let _guard = test_lock().lock().unwrap();
    drain();
    assert!(observations().lock().unwrap().is_empty());
}

#[test]
fn observations_accumulate_records() {
    let _guard = test_lock().lock().unwrap();
    drain();

    observations().lock().unwrap().push(ObservationRecord {
        text: "hello".to_string(),
        type_name: Some(TypeName("String".into())),
    });
    observations().lock().unwrap().push(ObservationRecord {
        text: "42".to_string(),
        type_name: None,
    });

    let obs = observations().lock().unwrap();
    assert_eq!(obs.len(), 2);
    assert_eq!(obs[0].text, "hello");
    assert!(obs[0].type_name.is_some());
    assert_eq!(obs[0].type_name.as_ref().unwrap().0, "String");
    assert_eq!(obs[1].text, "42");
    assert!(obs[1].type_name.is_none());
}

#[test]
fn observations_same_static_instance() {
    let _guard = test_lock().lock().unwrap();
    drain();

    let a = observations() as *const _;
    let b = observations() as *const _;
    assert_eq!(a, b);
}

#[test]
fn observations_drain_take_all() {
    let _guard = test_lock().lock().unwrap();
    drain();

    observations().lock().unwrap().push(ObservationRecord {
        text: "one".to_string(),
        type_name: None,
    });

    // Simulate the drain pattern used by Session.
    let drained: Vec<_> = observations().lock().unwrap().drain(..).collect();
    assert_eq!(drained.len(), 1);
    assert!(observations().lock().unwrap().is_empty());
}

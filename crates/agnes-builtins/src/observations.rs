use agnes_types::TypeName;
use std::sync::{Mutex, OnceLock};

/// A single recorded observation from a `tool_observe` special form.
pub struct ObservationRecord {
    pub text: String,
    pub type_name: Option<TypeName>,
}

/// Per-process recording of every `tool_observe` snapshot, drained by
/// `agnes_session::Session::run_turn` at the end of each turn and emitted
/// as a `SessionEvent::ObservationSummary`. Call sites should NOT rely on
/// this list accumulating across turns — Session takes ownership of the
/// entries on both the success and failure paths.
pub fn observations() -> &'static Mutex<Vec<ObservationRecord>> {
    static OBS: OnceLock<Mutex<Vec<ObservationRecord>>> = OnceLock::new();
    OBS.get_or_init(|| Mutex::new(Vec::new()))
}

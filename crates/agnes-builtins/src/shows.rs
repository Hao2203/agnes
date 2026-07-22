//! Show implementations for built-in types. Registered by
//! `register_builtins` after types themselves are registered.

use agnes_types::ShowFn;
use serde_json::Value as JsonValue;

pub fn path(v: &JsonValue) -> String {
    v.as_str().unwrap_or("").to_string()
}

pub fn string(v: &JsonValue) -> String {
    v.as_str().unwrap_or("").to_string()
}

pub fn int(v: &JsonValue) -> String {
    match v {
        JsonValue::Number(n) => n.to_string(),
        _ => v.to_string(),
    }
}

pub fn bool_(v: &JsonValue) -> String {
    match v {
        JsonValue::Bool(b) => b.to_string(),
        _ => v.to_string(),
    }
}

pub fn unit(_v: &JsonValue) -> String {
    // Unit collapses to empty. Contentful Unit payloads are also empty by
    // convention (Unit is the "no meaningful data" sentinel).
    String::new()
}

pub fn json(v: &JsonValue) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Type-erased list of `(name, ShowFn)` pairs to register.
pub const BUILTIN_SHOWS: &[(&str, ShowFn)] = &[
    ("JSON", json),
    ("Path", path),
    ("String", string),
    ("Int", int),
    ("Bool", bool_),
    ("Unit", unit),
];

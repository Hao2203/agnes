//! Show implementations for built-in types. Registered by
//! `register_builtins` after types themselves are registered.

use agnes_types::ShowFn;
use serde_json::Value as JsonValue;

/// Extract the JSON string, or the empty string when the value is null /
/// not a string. Used for text-shaped types where a stringly-typed data
/// payload is the norm.
pub fn as_str_or_empty(v: &JsonValue) -> &str {
    v.as_str().unwrap_or("")
}

pub fn plain_text(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn summary(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn markdown(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn html(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn path(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
}

pub fn string(v: &JsonValue) -> String {
    as_str_or_empty(v).to_string()
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

pub fn pdf(v: &JsonValue) -> String {
    let byte_count = v.as_str().map(|s| s.len()).unwrap_or(0);
    format!("<PDF binary, {byte_count} bytes>")
}

pub fn image(v: &JsonValue) -> String {
    let byte_count = v.as_str().map(|s| s.len()).unwrap_or(0);
    format!("<Image binary, {byte_count} bytes>")
}

/// Type-erased list of `(name, ShowFn)` pairs to register.
pub const BUILTIN_SHOWS: &[(&str, ShowFn)] = &[
    ("PlainText", plain_text),
    ("Summary", summary),
    ("Markdown", markdown),
    ("HTML", html),
    ("PDF", pdf),
    ("Image", image),
    ("JSON", json),
    ("Path", path),
    ("String", string),
    ("Int", int),
    ("Bool", bool_),
    ("Unit", unit),
];

//! Built-in types, aliases, and tool implementations for MVP.

mod types;
mod aliases;
mod tools;

pub use tools::{native_dispatch, BoxFuture, ToolImpl};

use agnes_registry::{Registry, RegistryError};
use agnes_types::{ToolSignature, TypeExpr, TypeName};

pub fn register_builtins(reg: &mut Registry) -> Result<(), RegistryError> {
    // --- Types + validators ---
    reg.register_type("Path",       Some(types::path_validator))?;
    reg.register_type("PlainText",  Some(types::utf8_validator))?;
    reg.register_type("Markdown",   Some(types::utf8_validator))?;
    reg.register_type("HTML",       Some(types::utf8_validator))?;
    reg.register_type("JSON",       Some(types::json_validator))?;
    reg.register_type("PDF",        Some(types::pdf_validator))?;
    reg.register_type("Image",      Some(types::image_validator))?;
    reg.register_type("Summary",    Some(types::utf8_validator))?;
    reg.register_type("Unit",       Some(types::unit_validator))?;
    reg.register_type("Unknown",    None)?;
    // Non-workflow types used by literals.
    reg.register_type("String",     None)?;
    reg.register_type("Int",        None)?;
    reg.register_type("Bool",       None)?;

    // --- Aliases ---
    reg.register_alias("TextLike",  aliases::text_like())?;
    reg.register_alias("VisualDoc", aliases::visual_doc())?;

    // --- Tools ---
    let path = TypeExpr::Named(TypeName("Path".into()));
    let plaintext = TypeExpr::Named(TypeName("PlainText".into()));
    let summary = TypeExpr::Named(TypeName("Summary".into()));
    let unit = TypeExpr::Named(TypeName("Unit".into()));
    let string_ty = TypeExpr::Named(TypeName("String".into()));

    reg.register_tool("read-file", ToolSignature {
        requires: vec![("path".into(), path.clone())],
        provides: plaintext.clone(),
    })?;
    reg.register_tool("write-file", ToolSignature {
        requires: vec![
            ("path".into(), path.clone()),
            ("content".into(), aliases::text_like()),
        ],
        provides: unit.clone(),
    })?;
    reg.register_tool("summarize", ToolSignature {
        requires: vec![("input".into(), TypeExpr::Union({
            let mut s = aliases::text_like().as_set();
            s.insert(TypeName("PDF".into()));
            s
        }))],
        provides: summary.clone(),
    })?;
    reg.register_tool("translate", ToolSignature {
        requires: vec![
            ("input".into(), aliases::text_like()),
            ("lang".into(), string_ty.clone()),
        ],
        provides: plaintext.clone(),
    })?;
    reg.register_tool("ocr", ToolSignature {
        requires: vec![("source".into(), aliases::visual_doc())],
        provides: plaintext.clone(),
    })?;
    reg.register_tool("llm", ToolSignature {
        requires: vec![
            ("prompt".into(), string_ty.clone()),
            ("input".into(), plaintext.clone()),
        ],
        provides: plaintext,
    })?;
    Ok(())
}

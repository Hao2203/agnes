//! Built-in types, aliases, and tool implementations for MVP.

mod aliases;
mod tools;
mod types;

pub use tools::{BoxFuture, ToolImpl, native_dispatch, writes};

use agnes_registry::{Registry, RegistryError};
use agnes_types::{ToolSignature, TypeExpr, TypeName, canonicalize_union};

pub fn register_builtins(reg: &mut Registry) -> Result<(), RegistryError> {
    // --- Types + validators ---
    reg.register_type("Path", Some(types::path_validator))?;
    reg.register_type("PlainText", Some(types::utf8_validator))?;
    reg.register_type("Markdown", Some(types::utf8_validator))?;
    reg.register_type("HTML", Some(types::utf8_validator))?;
    reg.register_type("JSON", Some(types::json_validator))?;
    reg.register_type("PDF", Some(types::pdf_validator))?;
    reg.register_type("Image", Some(types::image_validator))?;
    reg.register_type("Summary", Some(types::utf8_validator))?;
    reg.register_type("Unit", Some(types::unit_validator))?;
    reg.register_type("Unknown", None)?;
    // Non-workflow types used by literals.
    reg.register_type("String", None)?;
    reg.register_type("Int", None)?;
    reg.register_type("Bool", None)?;

    // --- Aliases ---
    reg.register_alias("TextLike", aliases::text_like())?;
    reg.register_alias("VisualDoc", aliases::visual_doc())?;

    // --- Tools ---
    let path = TypeExpr::named("Path");
    let plaintext = TypeExpr::named("PlainText");
    let summary = TypeExpr::named("Summary");
    let unit = TypeExpr::named("Unit");
    let string_ty = TypeExpr::named("String");

    reg.register_tool(
        "read-file",
        ToolSignature {
            requires: vec![("path".into(), path.clone())],
            provides: plaintext.clone(),
        },
    )?;
    reg.register_tool(
        "write-file",
        ToolSignature {
            requires: vec![
                ("path".into(), path.clone()),
                ("content".into(), aliases::text_like()),
            ],
            provides: unit.clone(),
        },
    )?;
    reg.register_tool(
        "summarize",
        ToolSignature {
            requires: vec![(
                "input".into(),
                canonicalize_union([
                    TypeExpr::named("PlainText"),
                    TypeExpr::named("Markdown"),
                    TypeExpr::named("HTML"),
                    TypeExpr::named("PDF"),
                ]),
            )],
            provides: summary.clone(),
        },
    )?;
    reg.register_tool(
        "translate",
        ToolSignature {
            requires: vec![
                ("input".into(), aliases::text_like()),
                ("lang".into(), string_ty.clone()),
            ],
            provides: plaintext.clone(),
        },
    )?;
    reg.register_tool(
        "ocr",
        ToolSignature {
            requires: vec![("source".into(), aliases::visual_doc())],
            provides: plaintext.clone(),
        },
    )?;
    reg.register_tool(
        "llm",
        ToolSignature {
            requires: vec![
                ("prompt".into(), string_ty.clone()),
                ("input".into(), plaintext.clone()),
            ],
            provides: plaintext.clone(),
        },
    )?;
    let text_or_md =
        canonicalize_union([TypeExpr::named("PlainText"), TypeExpr::named("Markdown")]);
    reg.register_tool(
        "join-lines",
        ToolSignature {
            requires: vec![(
                "lines".into(),
                TypeExpr::App {
                    head: TypeName("List".into()),
                    args: vec![text_or_md],
                },
            )],
            provides: plaintext.clone(),
        },
    )?;
    Ok(())
}

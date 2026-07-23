//! Built-in types, aliases, and tool implementations for MVP.

mod aliases;
mod shows;
mod tools;
mod types;

pub use tools::{BoxFuture, Tool, ToolCtx, ToolFn, ToolImpl, PathResolver, Sink, native_dispatch, writes};

use agnes_registry::{Registry, RegistryError};
use agnes_types::{ToolSignature, TypeExpr, TypeName};

pub fn register_builtins(reg: &mut Registry) -> Result<(), RegistryError> {
    // --- Types + validators ---
    reg.register_type("Path", Some(types::path_validator))?;
    reg.register_type("JSON", Some(types::json_validator))?;
    reg.register_type("Unit", Some(types::unit_validator))?;
    reg.register_type("Unknown", None)?;
    // Non-workflow types used by literals.
    reg.register_type("String", None)?;
    reg.register_type("Int", None)?;
    reg.register_type("Bool", None)?;
    reg.register_type("CommandResult", None)?;

    // --- Wrapper types (used at runtime by finish/observe) ---
    reg.register_type("Finish", None)?;
    reg.register_type("Observation", None)?;

    // --- Show impls for built-in types ---
    for (name, f) in shows::BUILTIN_SHOWS {
        reg.register_show(name, *f)?;
    }

    // --- Tools ---
    let path = TypeExpr::named("Path");
    let string = TypeExpr::named("String");
    let unit = TypeExpr::named("Unit");
    let command_result = TypeExpr::named("CommandResult");
    let list_string = TypeExpr::App {
        head: TypeName("List".into()),
        args: vec![string.clone()],
    };

    reg.register_tool(
        "read-file",
        ToolSignature {
            requires: vec![("path".into(), path.clone())],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "write-file",
        ToolSignature {
            requires: vec![
                ("path".into(), path.clone()),
                ("content".into(), string.clone()),
            ],
            provides: unit.clone(),
        },
    )?;
    reg.register_tool(
        "summarize",
        ToolSignature {
            requires: vec![("input".into(), string.clone())],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "translate",
        ToolSignature {
            requires: vec![
                ("lang".into(), string.clone()),
                ("input".into(), string.clone()),
            ],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "llm",
        ToolSignature {
            requires: vec![
                ("prompt".into(), string.clone()),
                ("input".into(), string.clone()),
            ],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "join-lines",
        ToolSignature {
            requires: vec![("lines".into(), list_string)],
            provides: string.clone(),
        },
    )?;
    reg.register_tool(
        "parse-path",
        ToolSignature {
            requires: vec![("path".into(), string.clone())],
            provides: path.clone(),
        },
    )?;

    // `finish` and `observe` are handled as special-form `Expr::Finish` /
    // `Expr::Observe` (parser -> checker -> compiler -> runtime); they are NOT
    // registered as tools. The wrapper types `Finish` / `Observation` remain
    // registered above so `show_value` and `classify_root` can recognise them.

    // shell-run tool
    reg.register_tool(
        "shell-run",
        ToolSignature {
            requires: vec![("command".into(), string)],
            provides: command_result,
        },
    )?;

    Ok(())
}

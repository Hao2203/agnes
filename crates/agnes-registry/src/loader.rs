use agnes_ast::{Param, Program, TopLevel};
use agnes_types::{ToolSignature, TypeExpr};

use crate::{Registry, RegistryError};

pub fn load(reg: &mut Registry, program: &Program) -> Result<(), RegistryError> {
    for tl in &program.toplevels {
        match tl {
            TopLevel::DeclareType { name, .. } => {
                reg.register_type(name, None)?;
            }
            TopLevel::DeclareTypeAlias { name, expr, .. } => {
                let resolved = reg.resolve(expr)?;
                reg.register_alias(name, resolved)?;
            }
            TopLevel::DeclareTool { name, requires, provides, .. } => {
                let sig = resolve_tool_sig(reg, requires, provides)?;
                // Allow override for user re-declares; but forbid initial dup.
                if reg.tool_signature(name).is_some() {
                    reg.override_tool(name, sig);
                } else {
                    reg.register_tool(name, sig)?;
                }
            }
            TopLevel::Define { name, params, provides, body, .. } => {
                // Register the tool signature so the checker sees this define
                // as a callable tool.
                let sig = resolve_tool_sig(reg, params, provides)?;
                if reg.tool_signature(name).is_some() {
                    reg.override_tool(name, sig);
                } else {
                    reg.register_tool(name, sig)?;
                }
                // Also store the body so the runtime can dispatch to it when
                // the builtin native-dispatch table misses.
                reg.register_define(name, params.clone(), (**body).clone());
            }
        }
    }
    Ok(())
}

pub fn resolve_tool_sig(
    reg: &Registry,
    requires: &[Param],
    provides: &agnes_ast::TypeExprAst,
) -> Result<ToolSignature, RegistryError> {
    let mut req = Vec::with_capacity(requires.len());
    for p in requires {
        req.push((p.name.clone(), reg.resolve(&p.ty)?));
    }
    let prov: TypeExpr = reg.resolve(provides)?;
    Ok(ToolSignature { requires: req, provides: prov })
}

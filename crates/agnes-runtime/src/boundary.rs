//! Recursive runtime boundary validation.
//!
//! `validate` walks the expected `TypeExpr` and enforces the JSON payload's
//! structural conformity. Named types run their registered `Validator`;
//! union types (`(| A B ...)`) pick the member matching the value's declared
//! type and recurse into it. Task 8 will add the List case; for now, other
//! App heads are treated as an internal-error condition.

use agnes_registry::Registry;
use agnes_types::{TypeExpr, TypeName, Value, type_expr_matches};

use crate::error::RuntimeError;

pub fn validate(
    reg: &Registry,
    tool: &str,
    direction: &'static str,
    expected: &TypeExpr,
    val: &Value,
) -> Result<(), RuntimeError> {
    match expected {
        TypeExpr::Named(n) => run_named_validator(reg, tool, direction, n, val),
        TypeExpr::App { head, args } if head.0 == "|" => {
            // Pick the union member matching the value's declared type and recurse.
            let matched = args
                .iter()
                .find(|m| type_expr_matches(&val.declared_type, m));
            match matched {
                Some(m) => validate(reg, tool, direction, m, val),
                None => Err(RuntimeError::RuntimeTypeError {
                    tool: tool.to_string(),
                    direction,
                    ty: TypeName(expected.to_string()),
                    cause: format!(
                        "value's declared type {} is not a member of expected union {}",
                        val.declared_type, expected
                    ),
                }),
            }
        }
        TypeExpr::App { head, .. } => {
            // Task 8 will add List. Any other head here is an internal error.
            Err(RuntimeError::RuntimeTypeError {
                tool: tool.to_string(),
                direction,
                ty: TypeName(head.0.clone()),
                cause: format!("unknown type constructor `{}` in canonical form", head.0),
            })
        }
    }
}

fn run_named_validator(
    reg: &Registry,
    tool: &str,
    direction: &'static str,
    ty: &TypeName,
    val: &Value,
) -> Result<(), RuntimeError> {
    if let Some(v) = reg.validator_of(ty) {
        v(&val.data).map_err(|cause| RuntimeError::RuntimeTypeError {
            tool: tool.to_string(),
            direction,
            ty: ty.clone(),
            cause,
        })?;
    }
    Ok(())
}

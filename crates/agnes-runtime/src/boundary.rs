//! Recursive runtime boundary validation.
//!
//! `validate` walks the expected `TypeExpr` and enforces the JSON payload's
//! structural conformity. Named types run their registered `Validator`;
//! union types (`(| A B ...)`) pick the member matching the value's declared
//! type and recurse into it. `(List T)` requires a JSON array and recurses
//! into each element against `T`. Other App heads remain an internal-error
//! condition (they shouldn't appear in canonical form).

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
            // Fast path: pick the union member matching the value's declared type
            // and recurse.
            if let Some(m) = args
                .iter()
                .find(|m| type_expr_matches(&val.declared_type, m))
            {
                return validate(reg, tool, direction, m, val);
            }
            // Fallback: declared_type didn't match a member directly (e.g. because
            // it's itself a union produced by list canonicalization). Try each
            // member's validator against the value's data. Accept the first that
            // passes; error only if none accept.
            for m in args {
                if validate(reg, tool, direction, m, val).is_ok() {
                    return Ok(());
                }
            }
            Err(RuntimeError::RuntimeTypeError {
                tool: tool.to_string(),
                direction,
                ty: TypeName(expected.to_string()),
                cause: format!(
                    "value's declared type {} is not a member of expected union {} and no member validator accepts the value",
                    val.declared_type, expected
                ),
            })
        }
        TypeExpr::App { head, args } if head.0 == "List" => {
            if args.len() != 1 {
                return Err(RuntimeError::RuntimeTypeError {
                    tool: tool.to_string(),
                    direction,
                    ty: TypeName(expected.to_string()),
                    cause: format!("List type has arity {}; expected 1", args.len()),
                });
            }
            let arr = val
                .data
                .as_array()
                .ok_or_else(|| RuntimeError::RuntimeTypeError {
                    tool: tool.to_string(),
                    direction,
                    ty: TypeName(expected.to_string()),
                    cause: format!("expected JSON array for List type, got {:?}", val.data),
                })?;
            let inner = &args[0];
            // The list value's own declared_type is `(List T_actual)`. Use T_actual
            // as each element's declared_type so unions in `expected` are resolved
            // by matching a concrete member. Fall back to the expected inner if the
            // outer value's shape is unexpected.
            let actual_inner = match &val.declared_type {
                TypeExpr::App {
                    head,
                    args: outer_args,
                } if head.0 == "List" && outer_args.len() == 1 => outer_args[0].clone(),
                _ => inner.clone(),
            };
            for (i, elem_data) in arr.iter().enumerate() {
                let elem_value = Value::typed_expr(elem_data.clone(), actual_inner.clone());
                validate(reg, tool, direction, inner, &elem_value).map_err(|e| {
                    // Wrap error to add element index for locatability.
                    match e {
                        RuntimeError::RuntimeTypeError {
                            tool,
                            direction,
                            ty,
                            cause,
                        } => RuntimeError::RuntimeTypeError {
                            tool,
                            direction,
                            ty,
                            cause: format!("element [{i}]: {cause}"),
                        },
                        other => other,
                    }
                })?;
            }
            Ok(())
        }
        TypeExpr::App { head, .. } => Err(RuntimeError::RuntimeTypeError {
            tool: tool.to_string(),
            direction,
            ty: TypeName(head.0.clone()),
            cause: format!("unknown type constructor `{}` in canonical form", head.0),
        }),
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

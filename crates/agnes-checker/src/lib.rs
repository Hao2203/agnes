//! Type checker for agnes DSL.
//!
//! Enforces:
//!   1. Parameter satisfaction: each argument's type satisfies the tool's require.
//!   2. Flow satisfaction: pipe upstream's provides satisfies downstream's require
//!      (when downstream is a single-param tool with an unbound positional slot).
//!
//! Both rules bottom out at `agnes_types::type_expr_matches`.

pub mod env;
pub mod error;

use agnes_ast::{Expr, Program, TopLevel};
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName, canonicalize_union, type_expr_matches};

pub use error::CheckError;

/// Top-level entry.
pub fn check(program: &Program, reg: &Registry) -> Result<(), CheckError> {
    for tl in &program.toplevels {
        if let TopLevel::Define {
            name,
            params,
            provides,
            body,
            ..
        } = tl
        {
            let mut env = env::Env::default();
            for p in params {
                let ty_expr = reg.resolve(&p.ty)?;
                env.set(p.name.clone(), ty_expr);
            }
            let body_ty = check_expr(body, reg, &mut env, None, None)?;
            let declared = reg.resolve(provides)?;
            if !type_expr_matches(&body_ty, &declared) {
                return Err(CheckError::DefineSignatureMismatch {
                    name: name.clone(),
                    declared: Box::new(declared),
                    body_type: Box::new(body_ty),
                });
            }
        }
    }
    if let Some(main) = &program.main {
        let mut env = env::Env::default();
        check_expr(main, reg, &mut env, None, None)?;
    }
    Ok(())
}

fn check_expr(
    e: &Expr,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeExpr>,
    hint: Option<&TypeExpr>,
) -> Result<TypeExpr, CheckError> {
    match e {
        Expr::Tool {
            name,
            positional,
            ..
        } => check_tool_call(name, positional, reg, env, flowed_in),
        Expr::Pipe { steps, .. } => {
            let mut upstream: Option<TypeExpr> = None;
            let mut last: Option<TypeExpr> = None;
            for step in steps {
                let ty = check_expr(step, reg, env, upstream.clone(), None)?;
                upstream = Some(ty.clone());
                last = Some(ty);
            }
            last.ok_or_else(|| CheckError::UnknownVar {
                name: "(empty pipe)".into(),
            })
        }
        Expr::Par { branches, .. } => {
            let mut last = None;
            for b in branches {
                last = Some(check_expr(b, reg, env, None, None)?);
            }
            last.ok_or_else(|| CheckError::UnknownVar {
                name: "(empty par)".into(),
            })
        }
        Expr::Let { name, value, .. } => {
            let bound = match value {
                None => flowed_in.clone().ok_or_else(|| CheckError::UnknownVar {
                    name: format!("(let {name}) with no upstream to name"),
                })?,
                Some(v) => check_expr(v, reg, env, None, None)?,
            };
            env.set(name.clone(), bound.clone());
            Ok(bound)
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            let _ = check_expr(cond, reg, env, None, None)?;
            let t = check_expr(then_branch, reg, env, None, None)?;
            let _ = check_expr(else_branch, reg, env, None, None)?;
            Ok(t)
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            let _ = check_expr(scrutinee, reg, env, None, None)?;
            let mut last = None;
            for (_, arm) in arms {
                last = Some(check_expr(arm, reg, env, None, None)?);
            }
            last.ok_or_else(|| CheckError::UnknownVar {
                name: "(empty match)".into(),
            })
        }
        Expr::Foreach {
            body, collection, ..
        } => {
            let _ = check_expr(collection, reg, env, None, None)?;
            check_expr(body, reg, env, None, None)
        }
        Expr::Retry { body, .. } => check_expr(body, reg, env, flowed_in, None),
        Expr::Catch { body, fallback, .. } => {
            let t = check_expr(body, reg, env, flowed_in.clone(), None)?;
            let _ = check_expr(fallback, reg, env, flowed_in, None)?;
            Ok(t)
        }
        Expr::Return { value, .. } => check_expr(value, reg, env, None, None),
        Expr::Finish { value, .. } => {
            // Reject if upstream is Observation.
            if let Some(ref up) = flowed_in {
                if matches!(up, TypeExpr::App { head, args } if args.len() == 1
                    && head.0 == "Observation")
                {
                    return Err(CheckError::FinishOnObservation {
                        found: up.to_string(),
                    });
                }
            }
            check_wrap(value, "Finish", "finish", reg, env, flowed_in)
        }
        Expr::Observe { value, .. } => {
            // Reject if upstream is Finish.
            if let Some(ref up) = flowed_in {
                if matches!(up, TypeExpr::App { head, args } if args.len() == 1
                    && head.0 == "Finish")
                {
                    return Err(CheckError::ObserveOnFinish {
                        found: up.to_string(),
                    });
                }
            }
            check_wrap(value, "Observation", "observe", reg, env, flowed_in)
        }
        Expr::Fmap { value, .. } => check_fmap(value, reg, env, flowed_in),
        Expr::ToolObserve {
            name,
            positional,
            ..
        } => check_tool_observe(name, positional, reg, env, flowed_in),
        Expr::Literal { lit, .. } => Ok(literal_type(lit)),
        Expr::Var { name, .. } => env
            .get(name)
            .cloned()
            .ok_or_else(|| CheckError::UnknownVar { name: name.clone() }),
        Expr::List { items, .. } => {
            if items.is_empty() {
                // Rule 3 (literal adaptation): if the caller passed a
                // structurally compatible hint `(List T)`, adopt it.
                if let Some(TypeExpr::App { head, args }) = hint
                    && head.0 == "List"
                    && args.len() == 1
                {
                    return Ok(TypeExpr::App {
                        head: head.clone(),
                        args: args.clone(),
                    });
                }
                return Ok(TypeExpr::App {
                    head: TypeName("List".into()),
                    args: vec![TypeExpr::Named(TypeName("Unknown".into()))],
                });
            }
            let mut elem_types: Vec<TypeExpr> = Vec::with_capacity(items.len());
            for it in items {
                elem_types.push(check_expr(it, reg, env, None, None)?);
            }
            let inner = canonicalize_union(elem_types);
            Ok(TypeExpr::App {
                head: TypeName("List".into()),
                args: vec![inner],
            })
        }
    }
}

fn literal_type(lit: &agnes_ast::Literal) -> TypeExpr {
    match lit {
        agnes_ast::Literal::String(_) => TypeExpr::Named(TypeName("String".into())),
        agnes_ast::Literal::Int(_) => TypeExpr::Named(TypeName("Int".into())),
        agnes_ast::Literal::Bool(_) => TypeExpr::Named(TypeName("Bool".into())),
        agnes_ast::Literal::Nil => TypeExpr::Named(TypeName("Unit".into())),
    }
}

/// Shared implementation for `(finish X)` / `(observe X)` type-checking.
/// Returns `App { head: wrapper_head, args: [inner_type] }` where `inner_type`
/// is the type of `X` (or the piped upstream, if `X` is None because the
/// form was written as a bare pipe step).
fn check_wrap(
    value: &Option<Box<Expr>>,
    wrapper_head: &str,
    form_name: &str,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeExpr>,
) -> Result<TypeExpr, CheckError> {
    let inner = match value {
        Some(v) => check_expr(v, reg, env, None, None)?,
        None => flowed_in.ok_or_else(|| CheckError::UnknownVar {
            name: format!("bare `{form_name}` used outside a pipe (no upstream to wrap)"),
        })?,
    };
    Ok(TypeExpr::App {
        head: TypeName(wrapper_head.into()),
        args: vec![inner],
    })
}

/// Type-check `(fmap expr)`. The upstream must be an Outcome
/// (`Observation T` or `Finish T`). Extract the inner `T`, check `expr`
/// with `T` as its upstream, and re-wrap the result in the same Outcome.
fn check_fmap(
    value: &Box<Expr>,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeExpr>,
) -> Result<TypeExpr, CheckError> {
    // Determine the upstream Outcome head and inner type.
    let (wrapper_head, inner_type) = match &flowed_in {
        Some(TypeExpr::App { head, args }) if args.len() == 1
            && (head.0 == "Observation" || head.0 == "Finish") =>
        {
            (head.clone(), args[0].clone())
        }
        Some(other) => {
            return Err(CheckError::FmapOnPlainType {
                found: other.to_string(),
            });
        }
        None => {
            return Err(CheckError::FmapOnPlainType {
                found: "<no upstream>".into(),
            });
        }
    };
    // Check the expression with the inner type as upstream.
    let inner_result = check_expr(value, reg, env, Some(inner_type), None)?;
    // Re-wrap in the same Outcome.
    Ok(TypeExpr::App {
        head: wrapper_head,
        args: vec![inner_result],
    })
}

/// Type-check `(tool_observe name args...)`. Behaves like `(tool name args...)`
/// followed by `observe`: the tool's provides is wrapped in `Observation T`.
/// The upstream must be plain (not an Outcome).
fn check_tool_observe(
    name: &str,
    positional: &[Expr],
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeExpr>,
) -> Result<TypeExpr, CheckError> {
    // Bare tool_observe in pipe tail: empty name, no positional args.
    // Just wrap the upstream in Observation.
    if name.is_empty() && positional.is_empty() {
        let inner = flowed_in.ok_or_else(|| CheckError::UnknownVar {
            name: "bare tool_observe used outside a pipe".into(),
        })?;
        // Reject if upstream is already an Outcome.
        if matches!(&inner, TypeExpr::App { head, args } if args.len() == 1
            && (head.0 == "Observation" || head.0 == "Finish"))
        {
            return Err(CheckError::ToolObserveOnOutcome {
                found: inner.to_string(),
            });
        }
        return Ok(TypeExpr::App {
            head: TypeName("Observation".into()),
            args: vec![inner],
        });
    }
    // Reject if upstream is an Outcome.
    if let Some(ref up) = flowed_in {
        if matches!(up, TypeExpr::App { head, args } if args.len() == 1
            && (head.0 == "Observation" || head.0 == "Finish"))
        {
            return Err(CheckError::ToolObserveOnOutcome {
                found: up.to_string(),
            });
        }
    }
    // Type-check like a tool call.
    let tool_type = check_tool_call(name, positional, reg, env, flowed_in)?;
    // Wrap in Observation.
    Ok(TypeExpr::App {
        head: TypeName("Observation".into()),
        args: vec![tool_type],
    })
}

fn check_arg(
    tool_name: &str,
    param: &str,
    expected: &TypeExpr,
    arg: &Expr,
    reg: &Registry,
    env: &mut env::Env,
) -> Result<(), CheckError> {
    if matches!(arg, Expr::Literal { .. }) {
        let _ = check_expr(arg, reg, env, None, None)?;
        return Ok(());
    }
    let actual = check_expr(arg, reg, env, None, Some(expected))?;
    if !type_expr_matches(&actual, expected) {
        return Err(CheckError::ParamMismatch {
            tool: tool_name.to_string(),
            param: param.to_string(),
            expected: Box::new(expected.clone()),
            actual: Box::new(actual),
        });
    }
    Ok(())
}

fn check_tool_call(
    tool_name: &str,
    positional: &[Expr],
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeExpr>,
) -> Result<TypeExpr, CheckError> {
    let sig: ToolSignature =
        reg.tool_signature(tool_name)
            .cloned()
            .ok_or_else(|| CheckError::UnknownTool {
                name: tool_name.to_string(),
            })?;

    let mut filled: Vec<bool> = vec![false; sig.requires.len()];

    for (i, pv) in positional.iter().enumerate() {
        if i >= sig.requires.len() {
            return Err(CheckError::UnknownVar {
                name: format!(
                    "extra positional arg at index {i} in call to `{tool_name}` (signature has {} required param(s))",
                    sig.requires.len()
                ),
            });
        }
        let (pname, param_expected) = sig.requires[i].clone();
        check_arg(tool_name, &pname, &param_expected, pv, reg, env)?;
        filled[i] = true;
    }

    let unfilled: Vec<usize> = filled
        .iter()
        .enumerate()
        .filter(|(_, b)| !**b)
        .map(|(i, _)| i)
        .collect();
    match (unfilled.len(), flowed_in) {
        (0, _) => {}
        (1, Some(up)) => {
            let (_, expected) = &sig.requires[unfilled[0]];
            if !type_expr_matches(&up, expected) {
                return Err(CheckError::FlowMismatch {
                    upstream: format!("<upstream (provides {up})>"),
                    downstream_tool: tool_name.to_string(),
                    expected: Box::new(expected.clone()),
                    actual: Box::new(up),
                });
            }
        }
        _ => {
            return Err(CheckError::UnknownVar {
                name: format!(
                    "tool `{tool_name}` has unfilled required params and no upstream to bind"
                ),
            });
        }
    }

    Ok(sig.provides.clone())
}

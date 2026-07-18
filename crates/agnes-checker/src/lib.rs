//! Type checker for agnes DSL.
//! Enforces exactly two rules:
//!   1. Parameter satisfaction: each argument's type is member of tool's require.
//!   2. Flow satisfaction: pipe upstream's provides is member of downstream's require
//!      (when downstream is a single-param tool with an unbound positional slot).

pub mod env;
pub mod error;

use agnes_ast::{Expr, Program, TopLevel};
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName, type_expr_matches};

pub use error::CheckError;

/// Top-level entry.
pub fn check(program: &Program, reg: &Registry) -> Result<(), CheckError> {
    // First, check every `define`'s body in isolation.
    for tl in &program.toplevels {
        if let TopLevel::Define { name, params, provides, body, .. } = tl {
            let mut env = env::Env::default();
            for p in params {
                let ty_expr = reg.resolve(&p.ty)?;
                let single = single_type(&ty_expr).ok_or_else(|| CheckError::UnknownVar {
                    name: format!("param `{}` must have a concrete or unioned type", p.name),
                })?;
                env.set(p.name.clone(), single);
            }
            let body_ty = check_expr(body, reg, &mut env, None)?;
            let declared = reg.resolve(provides)?;
            if !type_expr_matches(&body_ty, &declared) {
                return Err(CheckError::DefineSignatureMismatch {
                    name: name.clone(),
                    declared,
                    body_type: body_ty,
                });
            }
        }
    }
    // Then the main workflow, if any.
    if let Some(main) = &program.main {
        let mut env = env::Env::default();
        check_expr(main, reg, &mut env, None)?;
    }
    Ok(())
}

/// Walk an expression, returning the type it produces. `flowed_in` is the
/// upstream type (if we're inside a `pipe` and this expr is not the first).
fn check_expr(
    e: &Expr,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeName>,
) -> Result<TypeName, CheckError> {
    match e {
        Expr::Tool { name, positional, args, .. } => {
            check_tool_call(name, positional, args, reg, env, flowed_in)
        }
        Expr::Pipe { steps, .. } => {
            let mut upstream: Option<TypeName> = None;
            let mut last: Option<TypeName> = None;
            for step in steps {
                let ty = check_expr(step, reg, env, upstream.clone())?;
                upstream = Some(ty.clone());
                last = Some(ty);
            }
            last.ok_or_else(|| CheckError::UnknownVar { name: "(empty pipe)".into() })
        }
        Expr::Par { branches, .. } => {
            let mut last = None;
            for b in branches {
                last = Some(check_expr(b, reg, env, None)?);
            }
            last.ok_or_else(|| CheckError::UnknownVar { name: "(empty par)".into() })
        }
        Expr::Let { name, value, .. } => {
            let bound = match value {
                None => flowed_in.clone().ok_or_else(|| CheckError::UnknownVar {
                    name: format!("(let {name}) with no upstream to name"),
                })?,
                Some(v) => check_expr(v, reg, env, None)?,
            };
            env.set(name.clone(), bound.clone());
            Ok(bound)
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            let _ = check_expr(cond, reg, env, None)?;
            let t = check_expr(then_branch, reg, env, None)?;
            let _ = check_expr(else_branch, reg, env, None)?;
            Ok(t)
        }
        Expr::Match { scrutinee, arms, .. } => {
            let _ = check_expr(scrutinee, reg, env, None)?;
            let mut last = None;
            for (_, arm) in arms {
                last = Some(check_expr(arm, reg, env, None)?);
            }
            last.ok_or_else(|| CheckError::UnknownVar { name: "(empty match)".into() })
        }
        Expr::Foreach { body, collection, .. } => {
            let _ = check_expr(collection, reg, env, None)?;
            check_expr(body, reg, env, None)
        }
        Expr::Retry { body, .. } => check_expr(body, reg, env, flowed_in),
        Expr::Catch { body, fallback, .. } => {
            let t = check_expr(body, reg, env, flowed_in.clone())?;
            let _ = check_expr(fallback, reg, env, flowed_in)?;
            Ok(t)
        }
        Expr::Llm { positional, args, .. } => {
            // Walk sub-expressions so unknown vars / bad refs inside an llm call
            // are still surfaced. MVP: llm always provides PlainText.
            for pv in positional {
                let _ = check_expr(pv, reg, env, None)?;
            }
            for (_, v) in args {
                let _ = check_expr(v, reg, env, None)?;
            }
            Ok(TypeName("PlainText".into()))
        }
        Expr::Return { value, .. } => check_expr(value, reg, env, None),
        Expr::Literal { lit, .. } => Ok(literal_type(lit)),
        Expr::Var { name, .. } => env.get(name).cloned().ok_or_else(|| CheckError::UnknownVar {
            name: name.clone(),
        }),
    }
}

fn literal_type(lit: &agnes_ast::Literal) -> TypeName {
    match lit {
        agnes_ast::Literal::String(_) => TypeName("String".into()),
        agnes_ast::Literal::Int(_)    => TypeName("Int".into()),
        agnes_ast::Literal::Bool(_)   => TypeName("Bool".into()),
        agnes_ast::Literal::Nil       => TypeName("Unit".into()),
    }
}

fn single_type(t: &TypeExpr) -> Option<TypeName> {
    match t {
        TypeExpr::Named(n) => Some(n.clone()),
        TypeExpr::Union(_) => None,
    }
}

/// Check a single argument (positional or keyword) against a require's TypeExpr.
///
/// Literal arguments (String / Int / Bool / Nil) are admitted without type
/// matching — their conformance to the require's TypeExpr is a runtime
/// boundary validation, since the checker cannot know whether a `String`
/// literal is a valid `Path`, `Url`, etc. Non-literal args (variables, tool
/// results) must match structurally.
fn check_arg(
    tool_name: &str,
    param: &str,
    expected: &TypeExpr,
    arg: &Expr,
    reg: &Registry,
    env: &mut env::Env,
) -> Result<(), CheckError> {
    if matches!(arg, Expr::Literal { .. }) {
        // Walk it anyway so any nested checks happen (there are none for a
        // bare literal, but this keeps the pattern uniform).
        let _ = check_expr(arg, reg, env, None)?;
        return Ok(());
    }
    let actual = check_expr(arg, reg, env, None)?;
    if !type_expr_matches(&actual, expected) {
        return Err(CheckError::ParamMismatch {
            tool: tool_name.to_string(),
            param: param.to_string(),
            expected: expected.clone(),
            actual,
        });
    }
    Ok(())
}

/// Check a `(tool name pos... :kw v ...)` call.
///
/// Positional args bind to `sig.requires[i]` by index. Keyword args bind to
/// the require whose name matches the key. If exactly one required slot is
/// left unfilled after positional + keyword binding and we have a `flowed_in`
/// upstream (i.e. we're inside a `pipe`), the upstream fills that slot.
fn check_tool_call(
    tool_name: &str,
    positional: &[Expr],
    args: &agnes_ast::KwArgs,
    reg: &Registry,
    env: &mut env::Env,
    flowed_in: Option<TypeName>,
) -> Result<TypeName, CheckError> {
    let sig: ToolSignature = reg
        .tool_signature(tool_name)
        .cloned()
        .ok_or_else(|| CheckError::UnknownTool { name: tool_name.to_string() })?;

    // Track which sig params were filled.
    let mut filled: Vec<bool> = vec![false; sig.requires.len()];

    // 1. Positional args fill sig.requires in order.
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

    // 2. Keyword args fill by name.
    for (k, v) in args {
        let (idx, param_expected) = sig
            .requires
            .iter()
            .enumerate()
            .find(|(_, (n, _))| n == k)
            .map(|(i, (_, t))| (i, t.clone()))
            .ok_or_else(|| CheckError::UnknownVar {
                name: format!("keyword arg :{k} in call to `{tool_name}` not in signature"),
            })?;
        check_arg(tool_name, k, &param_expected, v, reg, env)?;
        filled[idx] = true;
    }

    // 3. If exactly one param is unfilled and we have flowed_in, bind it.
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
                    expected: expected.clone(),
                    actual: up,
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

    // Provides must be concrete (Named) for MVP flow — unions block dispatch.
    match sig.provides {
        TypeExpr::Named(n) => Ok(n),
        TypeExpr::Union(_) => Err(CheckError::UnknownVar {
            name: format!(
                "tool `{tool_name}` provides a Union type; MVP requires concrete provides"
            ),
        }),
    }
}

use std::collections::HashMap;

use agnes_ast::{Expr, KwArgs, Literal};
use agnes_builtins::ToolImpl;
use agnes_compiler::{Dag, Input, NodeId, NodeKind};
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName, Value};
use serde_json::Value as JsonValue;

use crate::boundary::validate;
use crate::error::RuntimeError;

/// Recursively evaluate the DAG root, returning the produced Value.
/// Results are memoized in `cache` so shared subgraphs execute once.
pub async fn run(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
) -> Result<Value, RuntimeError> {
    let mut cache: HashMap<NodeId, Value> = HashMap::new();
    let mut env: HashMap<String, Value> = HashMap::new();
    eval_node(dag, dag.root, reg, dispatch, &mut cache, &mut env).await
}

fn eval_node<'a>(
    dag: &'a Dag,
    id: NodeId,
    reg: &'a Registry,
    dispatch: &'a HashMap<String, ToolImpl>,
    cache: &'a mut HashMap<NodeId, Value>,
    env: &'a mut HashMap<String, Value>,
) -> agnes_builtins::BoxFuture<'a, Result<Value, RuntimeError>> {
    Box::pin(async move {
        if let Some(v) = cache.get(&id) {
            return Ok(v.clone());
        }
        let node = dag.get(id);
        let value = match &node.kind {
            NodeKind::Literal(lit) => Value {
                data: lit_to_json(lit),
                declared_type: lit_type(lit),
            },
            NodeKind::Var(name) => {
                env.get(name)
                    .cloned()
                    .ok_or_else(|| RuntimeError::ToolFailed {
                        tool: format!("<var>{name}"),
                        cause: "unbound variable".into(),
                    })?
            }
            NodeKind::Let { name } => {
                let src = eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?;
                env.insert(name.clone(), src.clone());
                src
            }
            NodeKind::Pipe => {
                // Evaluate every step in order so any `let` bindings placed in
                // intermediate steps populate `env` before later steps run.
                let mut last: Option<Value> = None;
                for input in &node.inputs {
                    last = Some(eval_input(dag, input, reg, dispatch, cache, env).await?);
                }
                last.ok_or_else(|| RuntimeError::ToolFailed {
                    tool: "<pipe>".into(),
                    cause: "empty pipe".into(),
                })?
            }
            NodeKind::Par => {
                // MVP: evaluate branches sequentially — correctness first;
                // concurrent join is a follow-up.
                for input in &node.inputs {
                    let _ = eval_input(dag, input, reg, dispatch, cache, env).await?;
                }
                Value {
                    data: JsonValue::Null,
                    declared_type: TypeName("Unit".into()),
                }
            }
            NodeKind::If => {
                let cond = eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?;
                let picked = if cond.data.as_bool().unwrap_or(false) {
                    1
                } else {
                    2
                };
                eval_input(dag, &node.inputs[picked], reg, dispatch, cache, env).await?
            }
            NodeKind::Match { arms } => {
                let s = eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?;
                let mut chosen: Option<usize> = None;
                for (i, pat) in arms.iter().enumerate() {
                    if lit_matches(pat, &s.data) {
                        chosen = Some(i + 1);
                        break;
                    }
                }
                let idx = chosen.unwrap_or(arms.len());
                let idx = idx.min(node.inputs.len() - 1);
                eval_input(dag, &node.inputs[idx], reg, dispatch, cache, env).await?
            }
            NodeKind::Foreach { .. } => {
                // MVP simplification: evaluate body once and return that.
                eval_input(dag, &node.inputs[1], reg, dispatch, cache, env).await?
            }
            NodeKind::Retry { times, .. } => {
                let mut last_err: Option<RuntimeError> = None;
                for _ in 0..(*times + 1) {
                    match eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await {
                        Ok(v) => {
                            cache.insert(id, v.clone());
                            return Ok(v);
                        }
                        Err(e) => last_err = Some(e),
                    }
                }
                return Err(last_err.unwrap());
            }
            NodeKind::Catch { fallback, .. } => {
                match eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await {
                    Ok(v) => v,
                    Err(_) => eval_node(dag, *fallback, reg, dispatch, cache, env).await?,
                }
            }
            NodeKind::Llm => {
                let args = collect_kwargs(dag, &node.inputs, reg, dispatch, cache, env).await?;
                call_native("llm", args, dispatch, reg, &node.provides).await?
            }
            NodeKind::Return => eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?,
            NodeKind::Tool { name } => {
                let args = collect_kwargs(dag, &node.inputs, reg, dispatch, cache, env).await?;
                call_native(name, args, dispatch, reg, &node.provides).await?
            }
        };
        cache.insert(id, value.clone());
        Ok(value)
    })
}

fn eval_input<'a>(
    dag: &'a Dag,
    input: &'a Input,
    reg: &'a Registry,
    dispatch: &'a HashMap<String, ToolImpl>,
    cache: &'a mut HashMap<NodeId, Value>,
    env: &'a mut HashMap<String, Value>,
) -> agnes_builtins::BoxFuture<'a, Result<Value, RuntimeError>> {
    Box::pin(async move {
        match input {
            Input::FromNode(id) => eval_node(dag, *id, reg, dispatch, cache, env).await,
            Input::Literal(lit) => Ok(Value {
                data: lit_to_json(lit),
                declared_type: lit_type(lit),
            }),
            Input::Var(name) => env
                .get(name)
                .cloned()
                .ok_or_else(|| RuntimeError::ToolFailed {
                    tool: format!("<var>{name}"),
                    cause: "unbound variable".into(),
                }),
            Input::Kw { source, .. } => eval_input(dag, source, reg, dispatch, cache, env).await,
        }
    })
}

async fn collect_kwargs(
    dag: &Dag,
    inputs: &[Input],
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    cache: &mut HashMap<NodeId, Value>,
    env: &mut HashMap<String, Value>,
) -> Result<HashMap<String, Value>, RuntimeError> {
    let mut out = HashMap::new();
    for input in inputs {
        match input {
            Input::Kw { key, source } => {
                let v = eval_input(dag, source, reg, dispatch, cache, env).await?;
                out.insert(key.clone(), v);
            }
            other => {
                // Defensive fallback — Task 7 compiler produces only Kw inputs
                // for tool/llm nodes, so this branch should not fire in normal use.
                let v = eval_input(dag, other, reg, dispatch, cache, env).await?;
                out.insert("_positional".into(), v);
            }
        }
    }
    Ok(out)
}

async fn call_native(
    tool: &str,
    args: HashMap<String, Value>,
    dispatch: &HashMap<String, ToolImpl>,
    reg: &Registry,
    provides: &TypeExpr,
) -> Result<Value, RuntimeError> {
    // Validate `requires` for every arg using the registry's structural validators.
    // Membership within a union has already been enforced by the checker; the
    // runtime's job is only to run each type's structural validator.
    if let Some(sig) = reg.tool_signature(tool) {
        for (k, expected) in &sig.requires {
            if let Some(v) = args.get(k) {
                validate(reg, tool, "requires", &v.declared_type, v)?;
                let _ = expected;
            }
        }
    }
    // Native dispatch first; on miss, fall back to a `define`d compound tool.
    let result = if let Some(f) = dispatch.get(tool) {
        f(args).await.map_err(|cause| RuntimeError::ToolFailed {
            tool: tool.to_string(),
            cause,
        })?
    } else if reg.define_body(tool).is_some() {
        dispatch_define(tool, args, reg, dispatch).await?
    } else {
        return Err(RuntimeError::MissingImpl {
            tool: tool.to_string(),
        });
    };
    // Validate `provides`.
    let ty: TypeName = match provides {
        TypeExpr::Named(n) => n.clone(),
        TypeExpr::Union(_) => result.declared_type.clone(),
    };
    validate(reg, tool, "provides", &ty, &result)?;
    Ok(result)
}

/// Bind incoming kwargs (with default-literal fallback) into a fresh env, then
/// interpret the define's body expression. Recursive via `call_native`.
fn dispatch_define<'a>(
    tool: &'a str,
    args: HashMap<String, Value>,
    reg: &'a Registry,
    dispatch: &'a HashMap<String, ToolImpl>,
) -> agnes_builtins::BoxFuture<'a, Result<Value, RuntimeError>> {
    Box::pin(async move {
        let (params, body) = reg
            .define_body(tool)
            .expect("dispatch_define called with a non-define tool name");
        let mut sub_env: HashMap<String, Value> = HashMap::new();
        for p in params {
            match args.get(&p.name) {
                Some(v) => {
                    sub_env.insert(p.name.clone(), v.clone());
                }
                None => {
                    if let Some(default) = &p.default {
                        sub_env.insert(
                            p.name.clone(),
                            Value {
                                data: lit_to_json(default),
                                declared_type: lit_type(default),
                            },
                        );
                    } else {
                        return Err(RuntimeError::ToolFailed {
                            tool: tool.to_string(),
                            cause: format!(
                                "missing required param `{}` to define `{tool}`",
                                p.name
                            ),
                        });
                    }
                }
            }
        }
        eval_expr(body, None, reg, dispatch, &mut sub_env).await
    })
}

/// Direct AST interpreter for `define` bodies. Mirrors the expression forms
/// handled by `agnes_checker::check_expr` but produces `Value`s at runtime.
/// Reused by recursive tool/llm calls via `call_native`.
fn eval_expr<'a>(
    e: &'a Expr,
    flowed_in: Option<Value>,
    reg: &'a Registry,
    dispatch: &'a HashMap<String, ToolImpl>,
    env: &'a mut HashMap<String, Value>,
) -> agnes_builtins::BoxFuture<'a, Result<Value, RuntimeError>> {
    Box::pin(async move {
        match e {
            Expr::Tool {
                name,
                positional,
                args,
                ..
            } => {
                let kwargs =
                    bind_tool_args(name, positional, args, flowed_in, reg, dispatch, env).await?;
                let provides = tool_provides(reg, name);
                call_native(name, kwargs, dispatch, reg, &provides).await
            }
            Expr::Pipe { steps, .. } => {
                let mut upstream: Option<Value> = None;
                for step in steps {
                    let v = eval_expr(step, upstream.clone(), reg, dispatch, env).await?;
                    upstream = Some(v);
                }
                upstream.ok_or_else(|| RuntimeError::ToolFailed {
                    tool: "<pipe>".into(),
                    cause: "empty pipe".into(),
                })
            }
            Expr::Par { branches, .. } => {
                for b in branches {
                    let _ = eval_expr(b, None, reg, dispatch, env).await?;
                }
                Ok(Value {
                    data: JsonValue::Null,
                    declared_type: TypeName("Unit".into()),
                })
            }
            Expr::Let { name, value, .. } => {
                let bound = match value {
                    Some(v) => eval_expr(v, None, reg, dispatch, env).await?,
                    None => flowed_in.clone().ok_or_else(|| RuntimeError::ToolFailed {
                        tool: format!("<let>{name}"),
                        cause: "(let ...) with no upstream to name".into(),
                    })?,
                };
                env.insert(name.clone(), bound.clone());
                Ok(bound)
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                let c = eval_expr(cond, None, reg, dispatch, env).await?;
                if c.data.as_bool().unwrap_or(false) {
                    eval_expr(then_branch, None, reg, dispatch, env).await
                } else {
                    eval_expr(else_branch, None, reg, dispatch, env).await
                }
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                let s = eval_expr(scrutinee, None, reg, dispatch, env).await?;
                for (pat, arm) in arms {
                    if lit_matches(pat, &s.data) {
                        return eval_expr(arm, None, reg, dispatch, env).await;
                    }
                }
                Ok(s)
            }
            Expr::Foreach {
                collection, body, ..
            } => {
                let _ = eval_expr(collection, None, reg, dispatch, env).await?;
                eval_expr(body, None, reg, dispatch, env).await
            }
            Expr::Retry { times, body, .. } => {
                let mut last_err: Option<RuntimeError> = None;
                for _ in 0..(*times + 1) {
                    match eval_expr(body, flowed_in.clone(), reg, dispatch, env).await {
                        Ok(v) => return Ok(v),
                        Err(e) => last_err = Some(e),
                    }
                }
                Err(last_err.unwrap())
            }
            Expr::Catch { body, fallback, .. } => {
                match eval_expr(body, flowed_in.clone(), reg, dispatch, env).await {
                    Ok(v) => Ok(v),
                    Err(_) => eval_expr(fallback, None, reg, dispatch, env).await,
                }
            }
            Expr::Llm {
                positional: _,
                args,
                ..
            } => {
                let mut kwargs: HashMap<String, Value> = HashMap::new();
                for (k, v) in args {
                    let val = eval_expr(v, None, reg, dispatch, env).await?;
                    kwargs.insert(k.clone(), val);
                }
                if let Some(up) = flowed_in
                    && !kwargs.contains_key("input")
                {
                    kwargs.insert("input".into(), up);
                }
                let provides = TypeExpr::Named(TypeName("PlainText".into()));
                call_native("llm", kwargs, dispatch, reg, &provides).await
            }
            Expr::Return { value, .. } => eval_expr(value, None, reg, dispatch, env).await,
            Expr::Literal { lit, .. } => Ok(Value {
                data: lit_to_json(lit),
                declared_type: lit_type(lit),
            }),
            Expr::Var { name, .. } => {
                env.get(name)
                    .cloned()
                    .ok_or_else(|| RuntimeError::ToolFailed {
                        tool: format!("<var>{name}"),
                        cause: "unbound variable".into(),
                    })
            }
        }
    })
}

/// Bind positional, keyword, and upstream arguments for a `(tool ...)` call in
/// the AST interpreter path. Mirrors `Lowering::lower_tool`.
async fn bind_tool_args(
    tool_name: &str,
    positional: &[Expr],
    args: &KwArgs,
    flowed_in: Option<Value>,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    env: &mut HashMap<String, Value>,
) -> Result<HashMap<String, Value>, RuntimeError> {
    let sig: ToolSignature =
        reg.tool_signature(tool_name)
            .cloned()
            .ok_or_else(|| RuntimeError::MissingImpl {
                tool: tool_name.to_string(),
            })?;

    let mut out: HashMap<String, Value> = HashMap::new();
    let mut filled: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (i, pv) in positional.iter().enumerate() {
        let (pname, _) = sig
            .requires
            .get(i)
            .ok_or_else(|| RuntimeError::ToolFailed {
                tool: tool_name.into(),
                cause: format!("extra positional arg at index {i}"),
            })?;
        let v = eval_expr(pv, None, reg, dispatch, env).await?;
        out.insert(pname.clone(), v);
        filled.insert(pname.clone());
    }
    for (k, v) in args {
        let val = eval_expr(v, None, reg, dispatch, env).await?;
        out.insert(k.clone(), val);
        filled.insert(k.clone());
    }
    let unfilled: Vec<&String> = sig
        .requires
        .iter()
        .map(|(n, _)| n)
        .filter(|n| !filled.contains(*n))
        .collect();
    if unfilled.len() == 1
        && let Some(up) = flowed_in
    {
        out.insert(unfilled[0].clone(), up);
    }
    Ok(out)
}

fn tool_provides(reg: &Registry, name: &str) -> TypeExpr {
    reg.tool_signature(name)
        .map(|s| s.provides.clone())
        .unwrap_or_else(|| TypeExpr::Named(TypeName("Unknown".into())))
}

fn lit_to_json(lit: &Literal) -> JsonValue {
    match lit {
        Literal::String(s) => JsonValue::String(s.clone()),
        Literal::Int(n) => JsonValue::from(*n),
        Literal::Bool(b) => JsonValue::Bool(*b),
        Literal::Nil => JsonValue::Null,
    }
}

fn lit_type(lit: &Literal) -> TypeName {
    match lit {
        Literal::String(_) => TypeName("String".into()),
        Literal::Int(_) => TypeName("Int".into()),
        Literal::Bool(_) => TypeName("Bool".into()),
        Literal::Nil => TypeName("Unit".into()),
    }
}

fn lit_matches(pat: &Literal, val: &JsonValue) -> bool {
    match (pat, val) {
        (Literal::String(a), JsonValue::String(b)) => a == b,
        (Literal::Int(a), JsonValue::Number(b)) => b.as_i64() == Some(*a),
        (Literal::Bool(a), JsonValue::Bool(b)) => a == b,
        (Literal::Nil, JsonValue::Null) => true,
        _ => false,
    }
}

use std::collections::HashMap;

use agnes_ast::{Expr, KwArgs, Literal};
use agnes_builtins::ToolImpl;
use agnes_compiler::{Dag, Input, NodeId, NodeKind};
use agnes_registry::Registry;
use agnes_types::{ToolSignature, TypeExpr, TypeName, Value, canonicalize_union};
use serde_json::Value as JsonValue;

use crate::boundary::validate;
use crate::error::RuntimeError;

use agnes_builtins::PathResolver;

/// Recursively evaluate the DAG root, returning the produced Value.
/// Results are memoized in `cache` so shared subgraphs execute once.
pub async fn run(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    resolver: &(dyn PathResolver + Send + Sync),
    tracer: &dyn crate::Tracer,
) -> Result<Value, RuntimeError> {
    let mut cache: HashMap<NodeId, Value> = HashMap::new();
    let mut env: HashMap<String, Value> = HashMap::new();
    eval_node(dag, dag.root, reg, dispatch, resolver, tracer, &mut cache, &mut env).await
}

fn eval_node<'a>(
    dag: &'a Dag,
    id: NodeId,
    reg: &'a Registry,
    dispatch: &'a HashMap<String, ToolImpl>,
    resolver: &'a (dyn PathResolver + Send + Sync),
    tracer: &'a dyn crate::Tracer,
    cache: &'a mut HashMap<NodeId, Value>,
    env: &'a mut HashMap<String, Value>,
) -> agnes_builtins::BoxFuture<'a, Result<Value, RuntimeError>> {
    Box::pin(async move {
        if let Some(v) = cache.get(&id) {
            return Ok(v.clone());
        }
        let node = dag.get(id);
        let value = match &node.kind {
            NodeKind::Literal(lit) => Value::typed(lit_to_json(lit), lit_type(lit)),
            NodeKind::Var(name) => {
                env.get(name)
                    .cloned()
                    .ok_or_else(|| RuntimeError::ToolFailed {
                        tool: format!("<var>{name}"),
                        cause: "unbound variable".into(),
                    })?
            }
            NodeKind::Let { name } => {
                let src =
                    eval_input(dag, &node.inputs[0], reg, dispatch, resolver, tracer, cache, env).await?;
                env.insert(name.clone(), src.clone());
                src
            }
            NodeKind::Pipe => {
                // Evaluate every step in order so any `let` bindings placed in
                // intermediate steps populate `env` before later steps run.
                let mut last: Option<Value> = None;
                for input in &node.inputs {
                    last = Some(eval_input(dag, input, reg, dispatch, resolver, tracer, cache, env).await?);
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
                    let _ = eval_input(dag, input, reg, dispatch, resolver, tracer, cache, env).await?;
                }
                Value::typed(JsonValue::Null, "Unit")
            }
            NodeKind::If => {
                let cond =
                    eval_input(dag, &node.inputs[0], reg, dispatch, resolver, tracer, cache, env).await?;
                let picked = if cond.data.as_bool().unwrap_or(false) {
                    1
                } else {
                    2
                };
                eval_input(dag, &node.inputs[picked], reg, dispatch, resolver, tracer, cache, env).await?
            }
            NodeKind::Match { arms } => {
                let s = eval_input(dag, &node.inputs[0], reg, dispatch, resolver, tracer, cache, env).await?;
                let mut chosen: Option<usize> = None;
                for (i, pat) in arms.iter().enumerate() {
                    if lit_matches(pat, &s.data) {
                        chosen = Some(i + 1);
                        break;
                    }
                }
                let idx = chosen.unwrap_or(arms.len());
                let idx = idx.min(node.inputs.len() - 1);
                eval_input(dag, &node.inputs[idx], reg, dispatch, resolver, tracer, cache, env).await?
            }
            NodeKind::Foreach { .. } => {
                // MVP simplification: evaluate body once and return that.
                eval_input(dag, &node.inputs[1], reg, dispatch, resolver, tracer, cache, env).await?
            }
            NodeKind::Retry { times, .. } => {
                let mut last_err: Option<RuntimeError> = None;
                for _ in 0..(*times + 1) {
                    match eval_input(dag, &node.inputs[0], reg, dispatch, resolver, tracer, cache, env).await
                    {
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
                match eval_input(dag, &node.inputs[0], reg, dispatch, resolver, tracer, cache, env).await {
                    Ok(v) => v,
                    Err(_) => eval_node(dag, *fallback, reg, dispatch, resolver, tracer, cache, env).await?,
                }
            }
            NodeKind::Llm => {
                let args =
                    collect_kwargs(dag, &node.inputs, reg, dispatch, resolver, tracer, cache, env).await?;
                call_native_traced(
                    id,
                    &node.kind,
                    "llm",
                    args,
                    dispatch,
                    resolver,
                    reg,
                    &node.provides,
                    tracer,
                )
                .await?
            }
            NodeKind::Return => {
                eval_input(dag, &node.inputs[0], reg, dispatch, resolver, tracer, cache, env).await?
            }
            NodeKind::Finish => {
                let inner =
                    eval_input(dag, &node.inputs[0], reg, dispatch, resolver, tracer, cache, env).await?;
                Value {
                    data: inner.data,
                    declared_type: TypeExpr::App {
                        head: TypeName("Finish".into()),
                        args: vec![inner.declared_type],
                    },
                }
            }
            NodeKind::Observe => {
                let inner =
                    eval_input(dag, &node.inputs[0], reg, dispatch, resolver, tracer, cache, env).await?;
                Value {
                    data: inner.data,
                    declared_type: TypeExpr::App {
                        head: TypeName("Observation".into()),
                        args: vec![inner.declared_type],
                    },
                }
            }
            NodeKind::Tool { name } => {
                let args =
                    collect_kwargs(dag, &node.inputs, reg, dispatch, resolver, tracer, cache, env).await?;
                call_native_traced(
                    id,
                    &node.kind,
                    name,
                    args,
                    dispatch,
                    resolver,
                    reg,
                    &node.provides,
                    tracer,
                )
                .await?
            }
            NodeKind::List => {
                let mut elems: Vec<Value> = Vec::with_capacity(node.inputs.len());
                for input in &node.inputs {
                    elems.push(eval_input(dag, input, reg, dispatch, resolver, tracer, cache, env).await?);
                }
                let data = JsonValue::Array(elems.iter().map(|v| v.data.clone()).collect());
                // Prefer the checker-derived provides for declared_type, but if
                // it's `(List Unknown)` (e.g. list contains Var inputs whose
                // static provides is Unknown), fall back to the runtime-observed
                // element types so downstream boundary validation can resolve
                // union members.
                let declared_type = match &node.provides {
                    TypeExpr::App { head, args } if head.0 == "List" && args.len() == 1 => {
                        let stale = matches!(&args[0], TypeExpr::Named(n) if n.0 == "Unknown");
                        if stale && !elems.is_empty() {
                            let elem_types: Vec<TypeExpr> =
                                elems.iter().map(|v| v.declared_type.clone()).collect();
                            TypeExpr::App {
                                head: TypeName("List".into()),
                                args: vec![canonicalize_union(elem_types)],
                            }
                        } else {
                            node.provides.clone()
                        }
                    }
                    _ => node.provides.clone(),
                };
                Value::typed_expr(data, declared_type)
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
    resolver: &'a (dyn PathResolver + Send + Sync),
    tracer: &'a dyn crate::Tracer,
    cache: &'a mut HashMap<NodeId, Value>,
    env: &'a mut HashMap<String, Value>,
) -> agnes_builtins::BoxFuture<'a, Result<Value, RuntimeError>> {
    Box::pin(async move {
        match input {
            Input::FromNode(id) => eval_node(dag, *id, reg, dispatch, resolver, tracer, cache, env).await,
            Input::Literal(lit) => Ok(Value::typed(lit_to_json(lit), lit_type(lit))),
            Input::Var(name) => env
                .get(name)
                .cloned()
                .ok_or_else(|| RuntimeError::ToolFailed {
                    tool: format!("<var>{name}"),
                    cause: "unbound variable".into(),
                }),
            Input::Kw { source, .. } => {
                eval_input(dag, source, reg, dispatch, resolver, tracer, cache, env).await
            }
        }
    })
}

async fn collect_kwargs(
    dag: &Dag,
    inputs: &[Input],
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    resolver: &(dyn PathResolver + Send + Sync),
    tracer: &dyn crate::Tracer,
    cache: &mut HashMap<NodeId, Value>,
    env: &mut HashMap<String, Value>,
) -> Result<HashMap<String, Value>, RuntimeError> {
    let mut out = HashMap::new();
    for input in inputs {
        match input {
            Input::Kw { key, source } => {
                let v = eval_input(dag, source, reg, dispatch, resolver, tracer, cache, env).await?;
                out.insert(key.clone(), v);
            }
            other => {
                // Defensive fallback — Task 7 compiler produces only Kw inputs
                // for tool/llm nodes, so this branch should not fire in normal use.
                let v = eval_input(dag, other, reg, dispatch, resolver, tracer, cache, env).await?;
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
    resolver: &(dyn PathResolver + Send + Sync),
    reg: &Registry,
    provides: &TypeExpr,
) -> Result<Value, RuntimeError> {
    // Validate `requires` for every arg using the registry's structural validators.
    // Membership within a union has already been enforced by the checker; the
    // runtime's job is only to run each type's structural validator.
    if let Some(sig) = reg.tool_signature(tool) {
        for (k, expected) in &sig.requires {
            if let Some(v) = args.get(k) {
                validate(reg, tool, "requires", expected, v)?;
            }
        }
    }
    // Native dispatch first; on miss, fall back to a `define`d compound tool.
    let result = if let Some(f) = dispatch.get(tool) {
        f.call(args, resolver).await.map_err(|cause| RuntimeError::ToolFailed {
            tool: tool.to_string(),
            cause,
        })?
    } else if reg.define_body(tool).is_some() {
        dispatch_define(tool, args, resolver, reg, dispatch).await?
    } else {
        return Err(RuntimeError::MissingImpl {
            tool: tool.to_string(),
        });
    };
    // Validate `provides`.
    validate(reg, tool, "provides", provides, &result)?;
    Ok(result)
}

// Tracing wrapper around `call_native`: bundles the identity/args/deps needed
// to invoke a native tool with the identity/duration needed to emit a trace
// pair. Splitting the args into a struct just to satisfy the linter would
// obscure the call sites, so we accept the extra parameter here.
#[allow(clippy::too_many_arguments)]
async fn call_native_traced(
    id: NodeId,
    kind: &NodeKind,
    tool: &str,
    args: HashMap<String, Value>,
    dispatch: &HashMap<String, ToolImpl>,
    resolver: &(dyn PathResolver + Send + Sync),
    reg: &Registry,
    provides: &TypeExpr,
    tracer: &dyn crate::Tracer,
) -> Result<Value, RuntimeError> {
    let preview = args_preview(&args);
    tracer.node_start(id, kind, &preview);
    let start = std::time::Instant::now();
    let out = call_native(tool, args, dispatch, resolver, reg, provides).await;
    let elapsed = start.elapsed();
    match &out {
        Ok(v) => tracer.node_end(id, Ok(v), elapsed),
        Err(e) => tracer.node_end(id, Err(e), elapsed),
    }
    out
}

fn args_preview(args: &HashMap<String, Value>) -> String {
    let mut kvs: Vec<(&String, &Value)> = args.iter().collect();
    kvs.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = String::new();
    for (i, (k, v)) in kvs.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let val = if let Some(s) = v.data.as_str() {
            let trimmed: String = s.chars().take(40).collect();
            format!(":{k}={trimmed:?}")
        } else {
            format!(":{k}=<{}>", v.declared_type)
        };
        out.push_str(&val);
    }
    out
}

/// Bind incoming kwargs (with default-literal fallback) into a fresh env, then
/// interpret the define's body expression. Recursive via `call_native`.
fn dispatch_define<'a>(
    tool: &'a str,
    args: HashMap<String, Value>,
    resolver: &'a (dyn PathResolver + Send + Sync),
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
                            Value::typed(lit_to_json(default), lit_type(default)),
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
        eval_expr(body, None, reg, dispatch, resolver, &mut sub_env).await
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
    resolver: &'a (dyn PathResolver + Send + Sync),
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
                    bind_tool_args(name, positional, args, flowed_in, reg, dispatch, resolver, env).await?;
                let provides = tool_provides(reg, name);
                call_native(name, kwargs, dispatch, resolver, reg, &provides).await
            }
            Expr::Pipe { steps, .. } => {
                let mut upstream: Option<Value> = None;
                for step in steps {
                    let v = eval_expr(step, upstream.clone(), reg, dispatch, resolver, env).await?;
                    upstream = Some(v);
                }
                upstream.ok_or_else(|| RuntimeError::ToolFailed {
                    tool: "<pipe>".into(),
                    cause: "empty pipe".into(),
                })
            }
            Expr::Par { branches, .. } => {
                for b in branches {
                    let _ = eval_expr(b, None, reg, dispatch, resolver, env).await?;
                }
                Ok(Value::typed(JsonValue::Null, "Unit"))
            }
            Expr::Let { name, value, .. } => {
                let bound = match value {
                    Some(v) => eval_expr(v, None, reg, dispatch, resolver, env).await?,
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
                let c = eval_expr(cond, None, reg, dispatch, resolver, env).await?;
                if c.data.as_bool().unwrap_or(false) {
                    eval_expr(then_branch, None, reg, dispatch, resolver, env).await
                } else {
                    eval_expr(else_branch, None, reg, dispatch, resolver, env).await
                }
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                let s = eval_expr(scrutinee, None, reg, dispatch, resolver, env).await?;
                for (pat, arm) in arms {
                    if lit_matches(pat, &s.data) {
                        return eval_expr(arm, None, reg, dispatch, resolver, env).await;
                    }
                }
                Ok(s)
            }
            Expr::Foreach {
                collection, body, ..
            } => {
                let _ = eval_expr(collection, None, reg, dispatch, resolver, env).await?;
                eval_expr(body, None, reg, dispatch, resolver, env).await
            }
            Expr::Retry { times, body, .. } => {
                let mut last_err: Option<RuntimeError> = None;
                for _ in 0..(*times + 1) {
                    match eval_expr(body, flowed_in.clone(), reg, dispatch, resolver, env).await {
                        Ok(v) => return Ok(v),
                        Err(e) => last_err = Some(e),
                    }
                }
                Err(last_err.unwrap())
            }
            Expr::Catch { body, fallback, .. } => {
                match eval_expr(body, flowed_in.clone(), reg, dispatch, resolver, env).await {
                    Ok(v) => Ok(v),
                    Err(_) => eval_expr(fallback, None, reg, dispatch, resolver, env).await,
                }
            }
            Expr::Llm {
                positional: _,
                args,
                ..
            } => {
                let mut kwargs: HashMap<String, Value> = HashMap::new();
                for (k, v) in args {
                    let val = eval_expr(v, None, reg, dispatch, resolver, env).await?;
                    kwargs.insert(k.clone(), val);
                }
                if let Some(up) = flowed_in
                    && !kwargs.contains_key("input")
                {
                    kwargs.insert("input".into(), up);
                }
                let provides = TypeExpr::Named(TypeName("PlainText".into()));
                call_native("llm", kwargs, dispatch, resolver, reg, &provides).await
            }
            Expr::Return { value, .. } => eval_expr(value, None, reg, dispatch, resolver, env).await,
            Expr::Finish { value, .. } => {
                eval_wrap_expr(value.as_deref(), flowed_in, "Finish", reg, dispatch, resolver, env).await
            }
            Expr::Observe { value, .. } => {
                eval_wrap_expr(
                    value.as_deref(),
                    flowed_in,
                    "Observation",
                    reg,
                    dispatch,
                    resolver,
                    env,
                )
                .await
            }
            Expr::Literal { lit, .. } => Ok(Value::typed(lit_to_json(lit), lit_type(lit))),
            Expr::Var { name, .. } => {
                env.get(name)
                    .cloned()
                    .ok_or_else(|| RuntimeError::ToolFailed {
                        tool: format!("<var>{name}"),
                        cause: "unbound variable".into(),
                    })
            }
            Expr::List { items, .. } => {
                let mut elems: Vec<Value> = Vec::with_capacity(items.len());
                let mut elem_types: Vec<TypeExpr> = Vec::with_capacity(items.len());
                for it in items {
                    let v = eval_expr(it, None, reg, dispatch, resolver, env).await?;
                    elem_types.push(v.declared_type.clone());
                    elems.push(v);
                }
                let elem_ty = if elem_types.is_empty() {
                    TypeExpr::named("Unknown")
                } else {
                    canonicalize_union(elem_types)
                };
                let data = JsonValue::Array(elems.iter().map(|v| v.data.clone()).collect());
                Ok(Value::typed_expr(
                    data,
                    TypeExpr::App {
                        head: TypeName("List".into()),
                        args: vec![elem_ty],
                    },
                ))
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
    resolver: &(dyn PathResolver + Send + Sync),
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
        let v = eval_expr(pv, None, reg, dispatch, resolver, env).await?;
        out.insert(pname.clone(), v);
        filled.insert(pname.clone());
    }
    for (k, v) in args {
        let val = eval_expr(v, None, reg, dispatch, resolver, env).await?;
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

/// AST-interpreter helper for `Expr::Finish` / `Expr::Observe`. Evaluates the
/// child (or takes the upstream from a piped context if `value` is `None`),
/// then wraps the resulting `Value.declared_type` in `App { wrapper_head, [inner] }`.
fn eval_wrap_expr<'a>(
    value: Option<&'a Expr>,
    flowed_in: Option<Value>,
    wrapper_head: &'static str,
    reg: &'a Registry,
    dispatch: &'a HashMap<String, ToolImpl>,
    resolver: &'a (dyn PathResolver + Send + Sync),
    env: &'a mut HashMap<String, Value>,
) -> agnes_builtins::BoxFuture<'a, Result<Value, RuntimeError>> {
    Box::pin(async move {
        let inner = match value {
            Some(v) => eval_expr(v, None, reg, dispatch, resolver, env).await?,
            None => flowed_in.ok_or_else(|| RuntimeError::ToolFailed {
                tool: format!("<{}>", wrapper_head.to_lowercase()),
                cause: format!("bare `{wrapper_head}` used outside a pipe"),
            })?,
        };
        Ok(Value {
            data: inner.data,
            declared_type: TypeExpr::App {
                head: TypeName(wrapper_head.into()),
                args: vec![inner.declared_type],
            },
        })
    })
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

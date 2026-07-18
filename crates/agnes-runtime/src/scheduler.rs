use std::collections::HashMap;

use agnes_ast::Literal;
use agnes_builtins::ToolImpl;
use agnes_compiler::{Dag, Input, NodeId, NodeKind};
use agnes_registry::Registry;
use agnes_types::{TypeExpr, TypeName, Value};
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
            NodeKind::Var(name) => env.get(name).cloned().ok_or_else(|| {
                RuntimeError::ToolFailed {
                    tool: format!("<var>{name}"),
                    cause: "unbound variable".into(),
                }
            })?,
            NodeKind::Let { name } => {
                let src = eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?;
                env.insert(name.clone(), src.clone());
                src
            }
            NodeKind::Pipe => {
                eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?
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
                let picked = if cond.data.as_bool().unwrap_or(false) { 1 } else { 2 };
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
            NodeKind::Return => {
                eval_input(dag, &node.inputs[0], reg, dispatch, cache, env).await?
            }
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
            Input::Var(name) => env.get(name).cloned().ok_or_else(|| {
                RuntimeError::ToolFailed {
                    tool: format!("<var>{name}"),
                    cause: "unbound variable".into(),
                }
            }),
            Input::Kw { source, .. } => {
                eval_input(dag, source, reg, dispatch, cache, env).await
            }
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
    let f = dispatch
        .get(tool)
        .ok_or_else(|| RuntimeError::MissingImpl {
            tool: tool.to_string(),
        })?;
    let result = f(args).await.map_err(|cause| RuntimeError::ToolFailed {
        tool: tool.to_string(),
        cause,
    })?;
    // Validate `provides`.
    let ty: TypeName = match provides {
        TypeExpr::Named(n) => n.clone(),
        TypeExpr::Union(_) => result.declared_type.clone(),
    };
    validate(reg, tool, "provides", &ty, &result)?;
    Ok(result)
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

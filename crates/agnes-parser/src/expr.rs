use agnes_ast::{Expr, Literal, Span};

use crate::error::ParseError;

pub fn parse_expr(v: &lexpr::Value, span: Span) -> Result<Expr, ParseError> {
    // Atoms
    if let Some(sym) = v.as_symbol() {
        // The bare symbol `nil` is the nil literal, not a variable reference.
        // lexpr's `Value::Nil` is only produced for `#nil`; a plain `nil`
        // token surfaces as a symbol. Normalize it here so the checker and
        // compiler see `Literal::Nil` uniformly.
        if sym == "nil" {
            return Ok(Expr::Literal {
                span,
                lit: Literal::Nil,
            });
        }
        // Any other bare symbol is a variable reference. (A bare symbol only
        // means "zero-arg tool call" as a step inside a `(pipe ...)`, which is
        // handled specially in `parse_pipe_steps`.)
        return Ok(Expr::Var {
            span,
            name: sym.to_string(),
        });
    }
    if let Some(s) = v.as_str() {
        return Ok(Expr::Literal {
            span,
            lit: Literal::String(s.to_string()),
        });
    }
    if let Some(b) = v.as_bool() {
        return Ok(Expr::Literal {
            span,
            lit: Literal::Bool(b),
        });
    }
    if let Some(n) = v.as_i64() {
        return Ok(Expr::Literal {
            span,
            lit: Literal::Int(n),
        });
    }
    if v.is_null() {
        return Ok(Expr::Literal {
            span,
            lit: Literal::Nil,
        });
    }

    // Compound
    let items = to_items(v, span)?;
    let head = items
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or_else(|| ParseError {
            span,
            message: "expression must start with a symbol".into(),
        })?;
    let rest = &items[1..];
    match head {
        "tool" => parse_tool(rest, span),
        "pipe" => Ok(Expr::Pipe {
            span,
            steps: parse_pipe_steps(rest, span)?,
        }),
        "par" => Ok(Expr::Par {
            span,
            branches: parse_exprs(rest, span)?,
        }),
        "list" => {
            let mut items = Vec::with_capacity(rest.len());
            for it in rest {
                items.push(parse_expr(it, span)?);
            }
            Ok(Expr::List { span, items })
        }
        "let" => parse_let(rest, span),
        "if" => parse_if(rest, span),
        "match" => parse_match(rest, span),
        "foreach" => parse_foreach(rest, span),
        "retry" => parse_retry(rest, span),
        "catch" => parse_catch(rest, span),
        "return" => {
            let inner = rest.first().ok_or_else(|| ParseError {
                span,
                message: "return needs an expression".into(),
            })?;
            Ok(Expr::Return {
                span,
                value: Box::new(parse_expr(inner, span)?),
            })
        }
        "finish" => parse_wrap_form(rest, span, "finish", |v| Expr::Finish {
            span,
            value: Some(Box::new(v)),
        }),
        "observe" => parse_wrap_form(rest, span, "observe", |v| Expr::Observe {
            span,
            value: Some(Box::new(v)),
        }),
        other => Err(ParseError {
            span,
            message: format!("unknown expression head `{other}`"),
        }),
    }
}

fn to_items(v: &lexpr::Value, span: Span) -> Result<Vec<lexpr::Value>, ParseError> {
    if v.is_null() {
        return Ok(vec![]);
    }
    if let Some(slice) = v.as_slice() {
        return Ok(slice.to_vec());
    }
    v.to_vec().ok_or_else(|| ParseError {
        span,
        message: format!("expected list, got {v:?}"),
    })
}

fn parse_exprs(items: &[lexpr::Value], span: Span) -> Result<Vec<Expr>, ParseError> {
    items.iter().map(|i| parse_expr(i, span)).collect()
}

/// Parse the steps of a `(pipe ...)`. A pipe step that is a bare symbol (other
/// than `nil`) is shorthand for a zero-argument call that takes the upstream
/// piped value as its input — e.g. `(pipe "done" finish)`. Bare `finish` and
/// `observe` desugar to their special-form counterparts with `value: None`
/// (the compiler's pipe-threading fills that from upstream); any other bare
/// symbol desugars to a zero-arg tool call. Non-symbol steps are parsed as
/// normal expressions.
fn parse_pipe_steps(items: &[lexpr::Value], span: Span) -> Result<Vec<Expr>, ParseError> {
    items
        .iter()
        .map(|i| match i.as_symbol() {
            Some("finish") => Ok(Expr::Finish { span, value: None }),
            Some("observe") => Ok(Expr::Observe { span, value: None }),
            Some(sym) if sym != "nil" => Ok(Expr::Tool {
                span,
                name: sym.to_string(),
                positional: vec![],
            }),
            _ => parse_expr(i, span),
        })
        .collect()
}

/// Shared implementation for `(finish expr)` / `(observe expr)`: exactly one
/// child expression, no keyword args.
fn parse_wrap_form(
    rest: &[lexpr::Value],
    span: Span,
    head: &str,
    make: impl FnOnce(Expr) -> Expr,
) -> Result<Expr, ParseError> {
    if rest.len() != 1 {
        return Err(ParseError {
            span,
            message: format!(
                "`{head}` takes exactly one child expression; got {}",
                rest.len()
            ),
        });
    }
    let inner = parse_expr(&rest[0], span)?;
    Ok(make(inner))
}

fn parse_tool(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let name = rest
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or_else(|| ParseError {
            span,
            message: "tool name expected".into(),
        })?
        .to_string();
    let positional = parse_exprs(&rest[1..], span)?;
    Ok(Expr::Tool {
        span,
        name,
        positional,
    })
}

fn parse_let(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let name = rest
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or_else(|| ParseError {
            span,
            message: "let name expected".into(),
        })?
        .to_string();
    let value = match rest.get(1) {
        None => None,
        Some(v) => Some(Box::new(parse_expr(v, span)?)),
    };
    Ok(Expr::Let { span, name, value })
}

fn parse_if(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    if rest.len() != 3 {
        return Err(ParseError {
            span,
            message: "if needs (cond then else)".into(),
        });
    }
    Ok(Expr::If {
        span,
        cond: Box::new(parse_expr(&rest[0], span)?),
        then_branch: Box::new(parse_expr(&rest[1], span)?),
        else_branch: Box::new(parse_expr(&rest[2], span)?),
    })
}

fn parse_match(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let scrutinee_val = rest.first().ok_or_else(|| ParseError {
        span,
        message: "match needs a scrutinee".into(),
    })?;
    let scrutinee = Box::new(parse_expr(scrutinee_val, span)?);
    let mut arms = Vec::new();
    for arm in &rest[1..] {
        let pair = to_items(arm, span)?;
        if pair.len() != 2 {
            return Err(ParseError {
                span,
                message: "match arm must be (pattern expr)".into(),
            });
        }
        let pat = literal_of(&pair[0], span)?;
        let body = parse_expr(&pair[1], span)?;
        arms.push((pat, body));
    }
    Ok(Expr::Match {
        span,
        scrutinee,
        arms,
    })
}

fn parse_foreach(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    if rest.len() != 3 {
        return Err(ParseError {
            span,
            message: "foreach needs (item collection body)".into(),
        });
    }
    let item = rest[0]
        .as_symbol()
        .ok_or_else(|| ParseError {
            span,
            message: "foreach item name (symbol) expected".into(),
        })?
        .to_string();
    Ok(Expr::Foreach {
        span,
        item,
        collection: Box::new(parse_expr(&rest[1], span)?),
        body: Box::new(parse_expr(&rest[2], span)?),
    })
}

fn parse_retry(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let mut times = None;
    let mut backoff = None;
    let mut body_val: Option<&lexpr::Value> = None;
    let mut i = 0usize;
    while i < rest.len() {
        if let Some(k) = rest[i].as_keyword() {
            let v = rest.get(i + 1).ok_or_else(|| ParseError {
                span,
                message: format!("keyword :{k} without value"),
            })?;
            match k {
                "times" => times = v.as_i64().map(|n| n as u32),
                "backoff" => backoff = v.as_str().map(str::to_string),
                other => {
                    return Err(ParseError {
                        span,
                        message: format!("unknown keyword :{other} in retry"),
                    });
                }
            }
            i += 2;
        } else {
            body_val = Some(&rest[i]);
            i += 1;
        }
    }
    let times = times.ok_or_else(|| ParseError {
        span,
        message: ":times required".into(),
    })?;
    let body_val = body_val.ok_or_else(|| ParseError {
        span,
        message: "retry body missing".into(),
    })?;
    Ok(Expr::Retry {
        span,
        times,
        backoff,
        body: Box::new(parse_expr(body_val, span)?),
    })
}

fn parse_catch(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let mut on = None;
    let mut fallback = None;
    let mut body_val: Option<&lexpr::Value> = None;
    let mut i = 0usize;
    while i < rest.len() {
        if let Some(k) = rest[i].as_keyword() {
            let v = rest.get(i + 1).ok_or_else(|| ParseError {
                span,
                message: format!("keyword :{k} without value"),
            })?;
            match k {
                "on" => on = v.as_symbol().map(str::to_string),
                "fallback" => fallback = Some(parse_expr(v, span)?),
                other => {
                    return Err(ParseError {
                        span,
                        message: format!("unknown keyword :{other} in catch"),
                    });
                }
            }
            i += 2;
        } else {
            body_val = Some(&rest[i]);
            i += 1;
        }
    }
    let fallback = fallback.ok_or_else(|| ParseError {
        span,
        message: ":fallback required in catch".into(),
    })?;
    let body_val = body_val.ok_or_else(|| ParseError {
        span,
        message: "catch body missing".into(),
    })?;
    Ok(Expr::Catch {
        span,
        on,
        fallback: Box::new(fallback),
        body: Box::new(parse_expr(body_val, span)?),
    })
}

fn literal_of(v: &lexpr::Value, span: Span) -> Result<Literal, ParseError> {
    match v {
        lexpr::Value::String(s) => Ok(Literal::String(s.to_string())),
        lexpr::Value::Number(_) => v.as_i64().map(Literal::Int).ok_or_else(|| ParseError {
            span,
            message: "only i64 int literals supported".into(),
        }),
        lexpr::Value::Bool(b) => Ok(Literal::Bool(*b)),
        lexpr::Value::Nil | lexpr::Value::Null => Ok(Literal::Nil),
        _ => Err(ParseError {
            span,
            message: "expected literal in match pattern".into(),
        }),
    }
}

use agnes_ast::{Literal, Param, Span, TopLevel, TypeExprAst};

use crate::UNION_BAR_SENTINEL;
use crate::error::ParseError;
use crate::expr;

pub fn parse_toplevel(form: &lexpr::Value, span: Span) -> Result<TopLevel, ParseError> {
    let items = as_list(form, span)?;
    // items[0] is 'declare' or 'define' (already checked by is_toplevel)
    let head = items[0].as_symbol().unwrap();
    match head {
        "declare" => parse_declare(&items[1..], span),
        "define" => parse_define(&items[1..], span),
        _ => unreachable!("is_toplevel gate"),
    }
}

fn parse_declare(rest: &[lexpr::Value], span: Span) -> Result<TopLevel, ParseError> {
    let kind = rest.first().and_then(|v| v.as_symbol()).ok_or_else(|| ParseError {
        span,
        message: "declare needs a kind: type | type-alias | tool".into(),
    })?;
    match kind {
        "type" => {
            let name = expect_symbol(rest.get(1), span, "type name")?;
            Ok(TopLevel::DeclareType { span, name: name.to_string() })
        }
        "type-alias" => {
            let name = expect_symbol(rest.get(1), span, "alias name")?;
            let expr_val = rest.get(2).ok_or_else(|| ParseError {
                span,
                message: "declare type-alias needs a body TypeExpr".into(),
            })?;
            let expr = parse_type_expr(expr_val, span)?;
            Ok(TopLevel::DeclareTypeAlias {
                span,
                name: name.to_string(),
                expr,
            })
        }
        "tool" => {
            let name = expect_symbol(rest.get(1), span, "tool name")?;
            let kw = parse_kwargs(&rest[2..], span)?;
            let requires_val = kw
                .iter()
                .find(|(k, _)| k == "requires")
                .map(|(_, v)| v.clone())
                .ok_or_else(|| ParseError {
                    span,
                    message: ":requires missing".into(),
                })?;
            let provides_val = kw
                .iter()
                .find(|(k, _)| k == "provides")
                .map(|(_, v)| v.clone())
                .ok_or_else(|| ParseError {
                    span,
                    message: ":provides missing".into(),
                })?;
            let requires = parse_params_vector(&requires_val, span)?;
            let provides = parse_type_expr(&provides_val, span)?;
            Ok(TopLevel::DeclareTool {
                span,
                name: name.to_string(),
                requires,
                provides,
            })
        }
        other => Err(ParseError {
            span,
            message: format!("unknown declare kind `{other}`; expected type | type-alias | tool"),
        }),
    }
}

fn parse_define(rest: &[lexpr::Value], span: Span) -> Result<TopLevel, ParseError> {
    let name = expect_symbol(rest.first(), span, "define name")?.to_string();
    let mut params: Vec<Param> = Vec::new();
    let mut provides: Option<TypeExprAst> = None;
    let mut body_val: Option<&lexpr::Value> = None;

    let mut i = 1usize;
    while i < rest.len() {
        if let Some(k) = rest[i].as_keyword() {
            let v = rest.get(i + 1).ok_or_else(|| ParseError {
                span,
                message: format!("keyword :{k} without value"),
            })?;
            match k {
                "params" => params = parse_params_vector(v, span)?,
                "provides" => provides = Some(parse_type_expr(v, span)?),
                other => {
                    return Err(ParseError {
                        span,
                        message: format!("unknown keyword :{other} in define"),
                    });
                }
            }
            i += 2;
        } else {
            body_val = Some(&rest[i]);
            i += 1;
        }
    }
    let provides = provides.ok_or_else(|| ParseError {
        span,
        message: ":provides missing in define".into(),
    })?;
    let body_val = body_val.ok_or_else(|| ParseError {
        span,
        message: "define body missing".into(),
    })?;
    let body = expr::parse_expr(body_val, span)?;
    Ok(TopLevel::Define {
        span,
        name,
        params,
        provides,
        body: Box::new(body),
    })
}

pub(crate) fn parse_params_vector(v: &lexpr::Value, span: Span) -> Result<Vec<Param>, ParseError> {
    let items = as_list(v, span)?;
    let mut out = Vec::new();
    for it in items {
        out.push(parse_single_param(&it, span)?);
    }
    Ok(out)
}

fn parse_single_param(v: &lexpr::Value, span: Span) -> Result<Param, ParseError> {
    // Syntax: (name: TypeExpr [:default Literal])
    let items = as_list(v, span)?;
    let raw_name = items
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or_else(|| ParseError {
            span,
            message: "param name symbol expected".into(),
        })?;
    if !raw_name.ends_with(':') {
        return Err(ParseError {
            span,
            message: format!("param name `{raw_name}` must end with ':'"),
        });
    }
    let name = raw_name.trim_end_matches(':').to_string();
    let ty_val = items.get(1).ok_or_else(|| ParseError {
        span,
        message: "param type expected after name".into(),
    })?;
    let ty = parse_type_expr(ty_val, span)?;
    let mut default = None;
    let mut i = 2usize;
    while i < items.len() {
        if let Some(k) = items[i].as_keyword() {
            let val = items.get(i + 1).ok_or_else(|| ParseError {
                span,
                message: format!("keyword :{k} in param without value"),
            })?;
            match k {
                "default" => default = Some(parse_literal(val, span)?),
                other => {
                    return Err(ParseError {
                        span,
                        message: format!("unknown keyword :{other} in param"),
                    });
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(Param { name, ty, default })
}

pub(crate) fn parse_type_expr(v: &lexpr::Value, span: Span) -> Result<TypeExprAst, ParseError> {
    if let Some(sym) = v.as_symbol() {
        if sym == UNION_BAR_SENTINEL {
            return Err(ParseError {
                span,
                message: "unexpected `|` in type position".into(),
            });
        }
        return Ok(TypeExprAst::Named(sym.to_string()));
    }
    // Otherwise it should be a list with '|' separators after preprocessing:
    // e.g. (PlainText __agnes_union_bar__ Markdown __agnes_union_bar__ HTML)
    let items = as_list(v, span)?;
    let mut members = Vec::new();
    let mut expect_type = true;
    for item in items {
        if expect_type {
            let sym = item.as_symbol().ok_or_else(|| ParseError {
                span,
                message: "type name (symbol) expected".into(),
            })?;
            if sym == UNION_BAR_SENTINEL {
                return Err(ParseError {
                    span,
                    message: "expected type name, got `|`".into(),
                });
            }
            members.push(TypeExprAst::Named(sym.to_string()));
            expect_type = false;
        } else {
            let sep = item.as_symbol().ok_or_else(|| ParseError {
                span,
                message: "expected `|` between type expressions".into(),
            })?;
            if sep != UNION_BAR_SENTINEL {
                return Err(ParseError {
                    span,
                    message: format!("expected `|`, got `{sep}`"),
                });
            }
            expect_type = true;
        }
    }
    if members.len() == 1 {
        Ok(members.into_iter().next().unwrap())
    } else {
        Ok(TypeExprAst::Union(members))
    }
}

fn parse_literal(v: &lexpr::Value, span: Span) -> Result<Literal, ParseError> {
    match v {
        lexpr::Value::String(s) => Ok(Literal::String(s.to_string())),
        lexpr::Value::Number(_) => v.as_i64().map(Literal::Int).ok_or_else(|| ParseError {
            span,
            message: "only i64 int literals supported in MVP".into(),
        }),
        lexpr::Value::Bool(b) => Ok(Literal::Bool(*b)),
        lexpr::Value::Nil | lexpr::Value::Null => Ok(Literal::Nil),
        _ => Err(ParseError {
            span,
            message: "expected literal".into(),
        }),
    }
}

pub(crate) fn parse_kwargs(
    items: &[lexpr::Value],
    span: Span,
) -> Result<Vec<(String, lexpr::Value)>, ParseError> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < items.len() {
        let k = items[i].as_keyword().ok_or_else(|| ParseError {
            span,
            message: format!("expected keyword arg, got {:?}", items[i]),
        })?;
        let v = items.get(i + 1).ok_or_else(|| ParseError {
            span,
            message: format!("keyword :{k} without value"),
        })?;
        out.push((k.to_string(), v.clone()));
        i += 2;
    }
    Ok(out)
}

fn expect_symbol<'a>(
    v: Option<&'a lexpr::Value>,
    span: Span,
    what: &str,
) -> Result<&'a str, ParseError> {
    v.and_then(|v| v.as_symbol()).ok_or_else(|| ParseError {
        span,
        message: format!("{what} (symbol) expected"),
    })
}

fn as_list(v: &lexpr::Value, span: Span) -> Result<Vec<lexpr::Value>, ParseError> {
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

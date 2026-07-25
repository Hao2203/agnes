//! Branch-pruned DSL pretty-printer.
//!
//! Renders an [`Expr`] back to agnes source, but prunes branches of
//! `if` and `match` expressions that were not actually executed. Which
//! nodes executed is indicated by the `visited_spans` set: any expression
//! whose span is in the set is rendered, others are either dropped or
//! replaced with `<not-run>`.

use std::collections::HashSet;

use crate::{Expr, Literal, Span};

/// Render an Expr as agnes source, pruning unvisited If/Match arms.
///
/// `visited_spans` is the set of spans for nodes that were actually executed.
/// Top-level nodes whose span is not in the set render to an empty string.
pub fn render_expr(e: &Expr, visited_spans: &HashSet<Span>) -> String {
    if !visited_spans.contains(&e.span()) {
        return String::new();
    }
    match e {
        Expr::Tool {
            name, positional, ..
        } => {
            let args: Vec<String> = positional
                .iter()
                .map(|a| render_expr(a, visited_spans))
                .collect();
            format!("(tool {} {})", name, args.join(" "))
        }
        Expr::Pipe { steps, .. } => {
            let rendered: Vec<String> = steps
                .iter()
                .map(|s| render_expr(s, visited_spans))
                .filter(|s| !s.is_empty())
                .collect();
            format!("(pipe {})", rendered.join(" "))
        }
        Expr::Par { branches, .. } => {
            let rendered: Vec<String> = branches
                .iter()
                .map(|b| render_expr(b, visited_spans))
                .filter(|s| !s.is_empty())
                .collect();
            format!("(par {})", rendered.join(" "))
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            let cond_rendered = render_expr(cond, visited_spans);
            let then_visited = visited_spans.contains(&then_branch.span());
            let else_visited = visited_spans.contains(&else_branch.span());
            let then_rendered = if then_visited {
                render_expr(then_branch, visited_spans)
            } else {
                "<not-run>".to_string()
            };
            let else_rendered = if else_visited {
                render_expr(else_branch, visited_spans)
            } else {
                "<not-run>".to_string()
            };
            format!("(if {} {} {})", cond_rendered, then_rendered, else_rendered)
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            let scrut_rendered = render_expr(scrutinee, visited_spans);
            let visited_arms: Vec<String> = arms
                .iter()
                .filter(|(_, body)| visited_spans.contains(&body.span()))
                .map(|(pat, body)| {
                    format!("({} {})", render_literal(pat), render_expr(body, visited_spans))
                })
                .collect();
            format!("(match {} {})", scrut_rendered, visited_arms.join(" "))
        }
        Expr::Fmap { value, .. } => {
            format!("(fmap {})", render_expr(value, visited_spans))
        }
        Expr::ToolObserve {
            name, positional, ..
        } => {
            if name.is_empty() && positional.is_empty() {
                "tool_observe".to_string()
            } else {
                let args: Vec<String> = positional
                    .iter()
                    .map(|a| render_expr(a, visited_spans))
                    .collect();
                format!("(tool_observe {} {})", name, args.join(" "))
            }
        }
        Expr::Finish { value, .. } => match value {
            Some(v) => format!("(finish {})", render_expr(v, visited_spans)),
            None => "finish".to_string(),
        },
        Expr::Observe { value, .. } => match value {
            Some(v) => format!("(observe {})", render_expr(v, visited_spans)),
            None => "observe".to_string(),
        },
        Expr::Let { name, value, .. } => match value {
            Some(v) => format!("(let {} {})", name, render_expr(v, visited_spans)),
            None => format!("(let {})", name),
        },
        Expr::Literal { lit, .. } => render_literal(lit),
        Expr::Var { name, .. } => name.clone(),
        Expr::List { items, .. } => {
            let rendered: Vec<String> = items
                .iter()
                .map(|i| render_expr(i, visited_spans))
                .filter(|s| !s.is_empty())
                .collect();
            format!("(list {})", rendered.join(" "))
        }
        Expr::Foreach {
            item,
            collection,
            body,
            ..
        } => {
            format!(
                "(foreach {} {} {})",
                item,
                render_expr(collection, visited_spans),
                render_expr(body, visited_spans)
            )
        }
        Expr::Retry {
            times,
            backoff,
            body,
            ..
        } => {
            let backoff_part = match backoff {
                Some(b) => format!(" :backoff {}", b),
                None => String::new(),
            };
            format!(
                "(retry :times {}{} {})",
                times,
                backoff_part,
                render_expr(body, visited_spans)
            )
        }
        Expr::Catch {
            on,
            fallback,
            body,
            ..
        } => {
            let on_part = match on {
                Some(o) => format!(" :on {}", o),
                None => String::new(),
            };
            format!(
                "(catch{} :fallback {} {})",
                on_part,
                render_expr(fallback, visited_spans),
                render_expr(body, visited_spans)
            )
        }
        Expr::Return { value, .. } => {
            format!("(return {})", render_expr(value, visited_spans))
        }
    }
}

fn render_literal(lit: &Literal) -> String {
    match lit {
        Literal::String(s) => format!("\"{}\"", s),
        Literal::Int(n) => format!("{}", n),
        Literal::Bool(b) => format!("{}", b),
        Literal::Nil => "nil".to_string(),
    }
}

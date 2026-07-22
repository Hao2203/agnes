use agnes_ast::{Expr, Program, TopLevel};
use std::collections::{HashMap, HashSet};

/// Detect a `define` transitively invoking itself (directly or through
/// other defines). Returns the name of the first cycle-owner found.
pub fn detect_define_cycles(program: &Program) -> Option<String> {
    let mut adj: HashMap<String, HashSet<String>> = HashMap::new();
    for tl in &program.toplevels {
        if let TopLevel::Define { name, body, .. } = tl {
            adj.insert(name.clone(), tool_names_in_expr(body));
        }
    }
    // Deterministic ordering: iterate the vector of define names in
    // declaration order rather than the HashMap's key iterator.
    for tl in &program.toplevels {
        if let TopLevel::Define { name, .. } = tl
            && reaches_self(name, &adj)
        {
            return Some(name.clone());
        }
    }
    None
}

fn tool_names_in_expr(e: &Expr) -> HashSet<String> {
    let mut out = HashSet::new();
    walk(e, &mut out);
    out
}

fn walk(e: &Expr, out: &mut HashSet<String>) {
    match e {
        Expr::Tool {
            name,
            positional,
            ..
        } => {
            out.insert(name.clone());
            for e in positional {
                walk(e, out);
            }
        }
        Expr::Pipe { steps, .. } => steps.iter().for_each(|s| walk(s, out)),
        Expr::Par { branches, .. } => branches.iter().for_each(|s| walk(s, out)),
        Expr::Let { value: Some(v), .. } => walk(v, out),
        Expr::Let { value: None, .. } => {}
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            walk(cond, out);
            walk(then_branch, out);
            walk(else_branch, out);
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            walk(scrutinee, out);
            for (_, a) in arms {
                walk(a, out);
            }
        }
        Expr::Foreach {
            collection, body, ..
        } => {
            walk(collection, out);
            walk(body, out);
        }
        Expr::Retry { body, .. } => walk(body, out),
        Expr::Catch { body, fallback, .. } => {
            walk(body, out);
            walk(fallback, out);
        }
        Expr::Return { value, .. } => walk(value, out),
        Expr::Finish { value, .. } | Expr::Observe { value, .. } => {
            if let Some(v) = value {
                walk(v, out);
            }
        }
        Expr::List { items, .. } => items.iter().for_each(|s| walk(s, out)),
        Expr::Literal { .. } | Expr::Var { .. } => {}
    }
}

fn reaches_self(start: &str, adj: &HashMap<String, HashSet<String>>) -> bool {
    let mut stack = vec![start.to_string()];
    let mut seen = HashSet::new();
    while let Some(cur) = stack.pop() {
        if let Some(neighbors) = adj.get(&cur) {
            for n in neighbors {
                if n == start {
                    return true;
                }
                if seen.insert(n.clone()) {
                    stack.push(n.clone());
                }
            }
        }
    }
    false
}

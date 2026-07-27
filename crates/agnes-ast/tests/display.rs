use std::collections::HashSet;

use agnes_ast::display::render_expr;
use agnes_ast::{Expr, Literal, Span};

fn dummy_span() -> Span {
    Span { start: 0, end: 0 }
}

fn visited(spans: &[Span]) -> HashSet<Span> {
    spans.iter().copied().collect()
}

// ---- Basic expression rendering -------------------------------------------

#[test]
fn renders_literals() {
    let s = dummy_span();
    let v = &visited(&[s]);

    let e = Expr::Literal {
        span: s,
        lit: Literal::String("hello".into()),
    };
    assert_eq!(render_expr(&e, v), "\"hello\"");

    let e = Expr::Literal {
        span: s,
        lit: Literal::Int(42),
    };
    assert_eq!(render_expr(&e, v), "42");

    let e = Expr::Literal {
        span: s,
        lit: Literal::Bool(true),
    };
    assert_eq!(render_expr(&e, v), "true");

    let e = Expr::Literal {
        span: s,
        lit: Literal::Nil,
    };
    assert_eq!(render_expr(&e, v), "nil");
}

#[test]
fn renders_var() {
    let s = dummy_span();
    let e = Expr::Var {
        span: s,
        name: "x".into(),
    };
    assert_eq!(render_expr(&e, &visited(&[s])), "x");
}

#[test]
fn renders_tool_call() {
    let s = dummy_span();
    let e = Expr::Tool {
        span: s,
        name: "read-file".into(),
        positional: vec![Expr::Literal {
            span: s,
            lit: Literal::String("x".into()),
        }],
    };
    assert_eq!(render_expr(&e, &visited(&[s])), "(tool read-file \"x\")");
}

#[test]
fn renders_pipe() {
    let s = dummy_span();
    let e = Expr::Pipe {
        span: s,
        steps: vec![
            Expr::Tool {
                span: s,
                name: "read-file".into(),
                positional: vec![Expr::Literal {
                    span: s,
                    lit: Literal::String("x".into()),
                }],
            },
            Expr::Tool {
                span: s,
                name: "summarize".into(),
                positional: vec![],
            },
        ],
    };
    assert_eq!(
        render_expr(&e, &visited(&[s])),
        "(pipe (tool read-file \"x\") (tool summarize ))"
    );
}

#[test]
fn renders_par() {
    let s = dummy_span();
    let e = Expr::Par {
        span: s,
        branches: vec![
            Expr::Var {
                span: s,
                name: "a".into(),
            },
            Expr::Var {
                span: s,
                name: "b".into(),
            },
        ],
    };
    assert_eq!(render_expr(&e, &visited(&[s])), "(par a b)");
}

#[test]
fn renders_list() {
    let s = dummy_span();
    let e = Expr::List {
        span: s,
        items: vec![
            Expr::Literal {
                span: s,
                lit: Literal::Int(1),
            },
            Expr::Literal {
                span: s,
                lit: Literal::Int(2),
            },
        ],
    };
    assert_eq!(render_expr(&e, &visited(&[s])), "(list 1 2)");
}

#[test]
fn renders_let_both_forms() {
    let s = dummy_span();
    let v = &visited(&[s]);

    let e = Expr::Let {
        span: s,
        name: "x".into(),
        value: None,
    };
    assert_eq!(render_expr(&e, v), "(let x)");

    let e = Expr::Let {
        span: s,
        name: "x".into(),
        value: Some(Box::new(Expr::Literal {
            span: s,
            lit: Literal::Int(1),
        })),
    };
    assert_eq!(render_expr(&e, v), "(let x 1)");
}

#[test]
fn renders_finish_both_forms() {
    let s = dummy_span();
    let v = &visited(&[s]);

    let e = Expr::Finish {
        span: s,
        value: None,
    };
    assert_eq!(render_expr(&e, v), "finish");

    let e = Expr::Finish {
        span: s,
        value: Some(Box::new(Expr::Literal {
            span: s,
            lit: Literal::String("done".into()),
        })),
    };
    assert_eq!(render_expr(&e, v), "(finish \"done\")");
}

#[test]
fn renders_observe_both_forms() {
    let s = dummy_span();
    let v = &visited(&[s]);

    let e = Expr::Observe {
        span: s,
        value: None,
    };
    assert_eq!(render_expr(&e, v), "observe");

    let e = Expr::Observe {
        span: s,
        value: Some(Box::new(Expr::Var {
            span: s,
            name: "x".into(),
        })),
    };
    assert_eq!(render_expr(&e, v), "(observe x)");
}

#[test]
fn renders_foreach() {
    let s = dummy_span();
    let e = Expr::Foreach {
        span: s,
        item: "x".into(),
        collection: Box::new(Expr::Var {
            span: s,
            name: "items".into(),
        }),
        body: Box::new(Expr::Tool {
            span: s,
            name: "process".into(),
            positional: vec![],
        }),
    };
    assert_eq!(
        render_expr(&e, &visited(&[s])),
        "(foreach x items (tool process ))"
    );
}

#[test]
fn renders_retry() {
    let s = dummy_span();
    let e = Expr::Retry {
        span: s,
        times: 3,
        backoff: None,
        body: Box::new(Expr::Tool {
            span: s,
            name: "fetch".into(),
            positional: vec![],
        }),
    };
    assert_eq!(
        render_expr(&e, &visited(&[s])),
        "(retry :times 3 (tool fetch ))"
    );
}

#[test]
fn renders_retry_with_backoff() {
    let s = dummy_span();
    let e = Expr::Retry {
        span: s,
        times: 3,
        backoff: Some("exponential".into()),
        body: Box::new(Expr::Tool {
            span: s,
            name: "fetch".into(),
            positional: vec![],
        }),
    };
    assert_eq!(
        render_expr(&e, &visited(&[s])),
        "(retry :times 3 :backoff exponential (tool fetch ))"
    );
}

#[test]
fn renders_catch() {
    let s = dummy_span();
    let e = Expr::Catch {
        span: s,
        on: None,
        fallback: Box::new(Expr::Literal {
            span: s,
            lit: Literal::Nil,
        }),
        body: Box::new(Expr::Tool {
            span: s,
            name: "risky".into(),
            positional: vec![],
        }),
    };
    assert_eq!(
        render_expr(&e, &visited(&[s])),
        "(catch :fallback nil (tool risky ))"
    );
}

#[test]
fn renders_catch_with_on() {
    let s = dummy_span();
    let e = Expr::Catch {
        span: s,
        on: Some("Transient".into()),
        fallback: Box::new(Expr::Literal {
            span: s,
            lit: Literal::Nil,
        }),
        body: Box::new(Expr::Tool {
            span: s,
            name: "risky".into(),
            positional: vec![],
        }),
    };
    assert_eq!(
        render_expr(&e, &visited(&[s])),
        "(catch :on Transient :fallback nil (tool risky ))"
    );
}

#[test]
fn renders_return() {
    let s = dummy_span();
    let e = Expr::Return {
        span: s,
        value: Box::new(Expr::Var {
            span: s,
            name: "x".into(),
        }),
    };
    assert_eq!(render_expr(&e, &visited(&[s])), "(return x)");
}

// ---- If / Match branch pruning -------------------------------------------

#[test]
fn if_then_branch_only() {
    let if_span = Span { start: 0, end: 10 };
    let cond_span = Span { start: 1, end: 2 };
    let then_span = Span { start: 3, end: 4 };
    let else_span = Span { start: 5, end: 6 };

    let e = Expr::If {
        span: if_span,
        cond: Box::new(Expr::Var {
            span: cond_span,
            name: "cond".into(),
        }),
        then_branch: Box::new(Expr::Var {
            span: then_span,
            name: "a".into(),
        }),
        else_branch: Box::new(Expr::Var {
            span: else_span,
            name: "b".into(),
        }),
    };

    let v = &visited(&[if_span, cond_span, then_span]);
    let out = render_expr(&e, v);
    assert!(out.contains("(if cond a <not-run>)"), "got: {}", out);
}

#[test]
fn if_else_branch_only() {
    let if_span = Span { start: 0, end: 10 };
    let cond_span = Span { start: 1, end: 2 };
    let then_span = Span { start: 3, end: 4 };
    let else_span = Span { start: 5, end: 6 };

    let e = Expr::If {
        span: if_span,
        cond: Box::new(Expr::Var {
            span: cond_span,
            name: "cond".into(),
        }),
        then_branch: Box::new(Expr::Var {
            span: then_span,
            name: "a".into(),
        }),
        else_branch: Box::new(Expr::Var {
            span: else_span,
            name: "b".into(),
        }),
    };

    let v = &visited(&[if_span, cond_span, else_span]);
    let out = render_expr(&e, v);
    assert!(
        out.contains("(if cond <not-run> b)"),
        "got: {}",
        out
    );
}

#[test]
fn if_both_branches_visited() {
    let if_span = Span { start: 0, end: 10 };
    let cond_span = Span { start: 1, end: 2 };
    let then_span = Span { start: 3, end: 4 };
    let else_span = Span { start: 5, end: 6 };

    let e = Expr::If {
        span: if_span,
        cond: Box::new(Expr::Var {
            span: cond_span,
            name: "cond".into(),
        }),
        then_branch: Box::new(Expr::Var {
            span: then_span,
            name: "a".into(),
        }),
        else_branch: Box::new(Expr::Var {
            span: else_span,
            name: "b".into(),
        }),
    };

    let v = &visited(&[if_span, cond_span, then_span, else_span]);
    let out = render_expr(&e, v);
    assert!(out.contains("(if cond a b)"), "got: {}", out);
}

#[test]
fn match_prunes_unvisited_arms() {
    let m_span = Span { start: 0, end: 20 };
    let scrut_span = Span { start: 1, end: 2 };
    let arm1_span = Span { start: 3, end: 4 };
    let arm2_span = Span { start: 5, end: 6 };
    let arm3_span = Span { start: 7, end: 8 };

    let e = Expr::Match {
        span: m_span,
        scrutinee: Box::new(Expr::Var {
            span: scrut_span,
            name: "x".into(),
        }),
        arms: vec![
            (
                Literal::Int(1),
                Expr::Var {
                    span: arm1_span,
                    name: "a".into(),
                },
            ),
            (
                Literal::Int(2),
                Expr::Var {
                    span: arm2_span,
                    name: "b".into(),
                },
            ),
            (
                Literal::Int(3),
                Expr::Var {
                    span: arm3_span,
                    name: "c".into(),
                },
            ),
        ],
    };

    // Only arm 2 visited
    let v = &visited(&[m_span, scrut_span, arm2_span]);
    let out = render_expr(&e, v);
    assert!(out.contains("(match x (2 b))"), "got: {}", out);
    assert!(!out.contains("(1"), "arm1 should be pruned: {}", out);
    assert!(!out.contains("(3"), "arm3 should be pruned: {}", out);
}

#[test]
fn match_multiple_visited_arms() {
    let m_span = Span { start: 0, end: 20 };
    let scrut_span = Span { start: 1, end: 2 };
    let arm1_span = Span { start: 3, end: 4 };
    let arm2_span = Span { start: 5, end: 6 };
    let arm3_span = Span { start: 7, end: 8 };

    let e = Expr::Match {
        span: m_span,
        scrutinee: Box::new(Expr::Var {
            span: scrut_span,
            name: "x".into(),
        }),
        arms: vec![
            (
                Literal::Int(1),
                Expr::Var {
                    span: arm1_span,
                    name: "a".into(),
                },
            ),
            (
                Literal::Int(2),
                Expr::Var {
                    span: arm2_span,
                    name: "b".into(),
                },
            ),
            (
                Literal::Int(3),
                Expr::Var {
                    span: arm3_span,
                    name: "c".into(),
                },
            ),
        ],
    };

    let v = &visited(&[m_span, scrut_span, arm1_span, arm3_span]);
    let out = render_expr(&e, v);
    assert!(out.contains("(1 a)"), "arm1 should be present: {}", out);
    assert!(out.contains("(3 c)"), "arm3 should be present: {}", out);
    assert!(!out.contains("(2"), "arm2 should be pruned: {}", out);
}

// ---- Fmap and ToolObserve -------------------------------------------------

#[test]
fn renders_fmap() {
    let s = dummy_span();
    let e = Expr::Fmap {
        span: s,
        value: Box::new(Expr::Tool {
            span: s,
            name: "summarize".into(),
            positional: vec![],
        }),
    };
    assert_eq!(
        render_expr(&e, &visited(&[s])),
        "(fmap (tool summarize ))"
    );
}

#[test]
fn fmap_renders_child_without_child_span_in_visited() {
    // fmap's child is evaluated via eval_expr (not through the DAG), so
    // its span is not in visited_spans. The child must still be rendered
    // rather than producing "(fmap )".
    let fmap_span = Span { start: 0, end: 10 };
    let child_span = Span { start: 5, end: 6 };

    let e = Expr::Fmap {
        span: fmap_span,
        value: Box::new(Expr::Var {
            span: child_span,
            name: "x".into(),
        }),
    };
    // Only the fmap span is visited; the child span is NOT.
    let out = render_expr(&e, &visited(&[fmap_span]));
    assert_eq!(out, "(fmap x)", "fmap child must render even when its span is not visited");
}

#[test]
fn renders_tool_observe_with_args() {
    let s = dummy_span();
    let e = Expr::ToolObserve {
        span: s,
        name: "read-file".into(),
        positional: vec![Expr::Literal {
            span: s,
            lit: Literal::String("x".into()),
        }],
    };
    assert_eq!(
        render_expr(&e, &visited(&[s])),
        "(tool_observe read-file \"x\")"
    );
}

#[test]
fn bare_tool_observe_in_pipe_tail() {
    let pipe_span = Span { start: 0, end: 10 };
    let step1_span = Span { start: 1, end: 2 };
    let step2_span = Span { start: 3, end: 4 };

    let e = Expr::Pipe {
        span: pipe_span,
        steps: vec![
            Expr::Tool {
                span: step1_span,
                name: "read-file".into(),
                positional: vec![Expr::Literal {
                    span: step1_span,
                    lit: Literal::String("x".into()),
                }],
            },
            Expr::ToolObserve {
                span: step2_span,
                name: String::new(),
                positional: vec![],
            },
        ],
    };

    let v = &visited(&[pipe_span, step1_span, step2_span]);
    let out = render_expr(&e, v);
    assert!(
        out.ends_with("tool_observe)"),
        "bare tool_observe should render as bare symbol, got: {}",
        out
    );
    assert!(
        !out.contains("(tool_observe"),
        "should not have tool_observe parens, got: {}",
        out
    );
}

// ---- Unvisited top-level returns empty -----------------------------------

#[test]
fn unvisited_expr_returns_empty() {
    let s = dummy_span();
    let e = Expr::Var {
        span: s,
        name: "x".into(),
    };
    assert_eq!(render_expr(&e, &HashSet::new()), "");
}

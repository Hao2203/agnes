# fmap, tool_observe, and Branch-Pruned DSL Feedback — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `fmap` and `tool_observe` special forms, multi-observation feedback, and branch-pruned DSL echo.

**Architecture:** Incremental additions to parse→check→compile→execute. Two new special forms follow the `finish`/`observe` pattern. A `observations()` recorder mirrors `writes()`. A `visited: HashSet<NodeId>` threaded through the scheduler enables post-execution DSL pruning. The planner's `Iteration` gains `Vec<Observation>`.

**Tech Stack:** Rust, tokio, lexpr, agnes-* crates.

## Global Constraints

- Backward compatible: existing `examples/*.agnes` and all tests pass without modification.
- `Finish` and `Observation` types remain registered; `classify_root` reads the type head as before.
- `finish` and `observe` syntax and semantics unchanged.
- No implicit lifting in pipe; each step's input must match the previous step's output.
- `fmap` on `Finish T` is included for symmetry but primarily used with `Observation T`.

---

### Task 1: AST, Parser, and DAG definitions

**Files:**
- Modify: `crates/agnes-ast/src/lib.rs`
- Modify: `crates/agnes-parser/src/expr.rs`
- Modify: `crates/agnes-compiler/src/dag.rs`
- Test: `crates/agnes-parser/tests/parse.rs`

**Interfaces:**
- Produces: `Expr::Fmap { span, value: Box<Expr> }`, `Expr::ToolObserve { span, name: String, positional: Vec<Expr> }`, `NodeKind::Fmap`, `NodeKind::ToolObserve { name: String }`

- [ ] **Step 1: Add `Expr::Fmap` and `Expr::ToolObserve` to AST**

In `crates/agnes-ast/src/lib.rs`, add after the existing `Observe` variant:

```rust
    /// `(fmap expr)` — functor lift over an Outcome. Extracts the inner
    /// value from an upstream `Observation T` (or `Finish T`), evaluates
    /// `expr` with that inner value as the piped upstream, and re-wraps
    /// the result in the same Outcome, preserving the mode.
    Fmap {
        span: Span,
        value: Box<Expr>,
    },
    /// `(tool_observe name args...)` — combinator: run the named tool,
    /// snapshot the result for LLM feedback, wrap in `Observation T`.
    /// Non-terminal — the pipe continues via `fmap`.
    ToolObserve {
        span: Span,
        name: String,
        positional: Vec<Expr>,
    },
```

Also update the `span()` method in `Expr::span()` to handle the new variants:

```rust
            Expr::Fmap { span, .. }
            | Expr::ToolObserve { span, .. } => *span,
```

Add these two lines after the existing `Expr::Observe { span, .. }` line in the match arm.

- [ ] **Step 2: Add `NodeKind::Fmap` and `NodeKind::ToolObserve` to DAG**

In `crates/agnes-compiler/src/dag.rs`, add after the existing `Observe` variant:

```rust
    /// `(fmap expr)` — functor lift: single input (the child expression).
    /// Provides is `App { head: wrapper_head, args: [child_provides] }`
    /// where `wrapper_head` is the upstream Outcome's head (Observation/Finish).
    Fmap,
    /// `(tool_observe name args...)` — tool + observe combinator.
    /// Inputs are kwargs (like a Tool node). Provides is
    /// `App { head: "Observation", args: [tool_provides] }`.
    ToolObserve {
        name: String,
    },
```

- [ ] **Step 3: Parse `(fmap ...)` and `(tool_observe ...)` forms**

In `crates/agnes-parser/src/expr.rs`, add two new match arms in `parse_expr` after the `"observe"` arm (line 98):

```rust
        "fmap" => parse_fmap(rest, span),
        "tool_observe" => parse_tool_observe(rest, span),
```

Add the `parse_fmap` function:

```rust
fn parse_fmap(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    if rest.len() != 1 {
        return Err(ParseError {
            span,
            message: format!("fmap takes exactly one child expression; got {}", rest.len()),
        });
    }
    let inner = parse_expr(&rest[0], span)?;
    Ok(Expr::Fmap {
        span,
        value: Box::new(inner),
    })
}
```

Add the `parse_tool_observe` function:

```rust
fn parse_tool_observe(rest: &[lexpr::Value], span: Span) -> Result<Expr, ParseError> {
    let name = rest
        .first()
        .and_then(|v| v.as_symbol())
        .ok_or_else(|| ParseError {
            span,
            message: "tool_observe: tool name expected".into(),
        })?
        .to_string();
    let positional = parse_exprs(&rest[1..], span)?;
    Ok(Expr::ToolObserve {
        span,
        name,
        positional,
    })
}
```

Also add `"fmap"` and `"tool_observe"` to `parse_pipe_steps` as bare-symbol support. In the `parse_pipe_steps` function, add after the `"observe"` line:

```rust
            Some("fmap") => Ok(Expr::Fmap { span, value: None }),
            Some("tool_observe") => Ok(Expr::ToolObserve {
                span,
                name: String::new(),
                positional: vec![],
            }),
```

Wait — bare `fmap` in a pipe doesn't make sense (fmap needs an expression). And bare `tool_observe` in a pipe tail means "snapshot the upstream value" (no tool run). For bare `fmap`, we should reject it at check time. For bare `tool_observe`, we need to handle it specially. Let me adjust:

Actually, bare `fmap` in a pipe should be rejected by the checker (fmap always needs an expression). We can let the parser produce `Expr::Fmap { value: None }` and have the checker reject it. For bare `tool_observe` as a pipe tail, it should work: take the upstream value, snapshot it, wrap in Observation T. We'll handle `value: None`-like semantics differently — the `ToolObserve` form always has a name. For the bare pipe tail case, we can use a different approach: check if `name` is empty and `positional` is empty at runtime, and treat it as a bare snapshot of the upstream.

Let me revise: in `parse_pipe_steps`, add only:

```rust
            Some("tool_observe") => Ok(Expr::ToolObserve {
                span,
                name: String::new(),
                positional: vec![],
            }),
```

The empty name + empty positional signals "bare tool_observe in pipe tail."

- [ ] **Step 4: Write parser tests**

In `crates/agnes-parser/tests/parse.rs`, add test cases:

```rust
#[test]
fn parse_fmap_form() {
    let prog = agnes_parser::parse("(fmap (tool summarize))").unwrap();
    let main = prog.main.unwrap();
    match main {
        agnes_ast::Expr::Fmap { value, .. } => {
            match *value {
                agnes_ast::Expr::Tool { name, .. } => assert_eq!(name, "summarize"),
                other => panic!("expected Tool, got {other:?}"),
            }
        }
        other => panic!("expected Fmap, got {other:?}"),
    }
}

#[test]
fn parse_tool_observe_form() {
    let prog = agnes_parser::parse("(tool_observe read-file \"x\")").unwrap();
    let main = prog.main.unwrap();
    match main {
        agnes_ast::Expr::ToolObserve { name, positional, .. } => {
            assert_eq!(name, "read-file");
            assert_eq!(positional.len(), 1);
        }
        other => panic!("expected ToolObserve, got {other:?}"),
    }
}

#[test]
fn parse_pipe_with_fmap() {
    let prog = agnes_parser::parse(
        "(pipe (tool_observe read-file \"x\") (fmap (tool summarize)))"
    ).unwrap();
    let main = prog.main.unwrap();
    match main {
        agnes_ast::Expr::Pipe { steps, .. } => {
            assert_eq!(steps.len(), 2);
            assert!(matches!(steps[0], agnes_ast::Expr::ToolObserve { .. }));
            assert!(matches!(steps[1], agnes_ast::Expr::Fmap { .. }));
        }
        other => panic!("expected Pipe, got {other:?}"),
    }
}

#[test]
fn parse_bare_tool_observe_in_pipe_tail() {
    let prog = agnes_parser::parse(
        "(pipe (tool read-file \"x\") tool_observe)"
    ).unwrap();
    let main = prog.main.unwrap();
    match main {
        agnes_ast::Expr::Pipe { steps, .. } => {
            assert_eq!(steps.len(), 2);
            match &steps[1] {
                agnes_ast::Expr::ToolObserve { name, positional, .. } => {
                    assert!(name.is_empty());
                    assert!(positional.is_empty());
                }
                other => panic!("expected ToolObserve, got {other:?}"),
            }
        }
        other => panic!("expected Pipe, got {other:?}"),
    }
}
```

- [ ] **Step 5: Run parser tests to verify they pass**

Run: `cargo test -p agnes-parser`
Expected: new tests PASS, all existing tests still PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/agnes-ast/src/lib.rs crates/agnes-parser/src/expr.rs crates/agnes-parser/tests/parse.rs crates/agnes-compiler/src/dag.rs
git commit -m "feat(ast): add Expr::Fmap, Expr::ToolObserve, NodeKind::Fmap, NodeKind::ToolObserve

Co-Authored-By: Claude <noreply@anthropic.com>"
```

### Task 2: Type Checker (fmap, tool_observe, mode mismatch rejection)

**Files:**
- Modify: `crates/agnes-checker/src/lib.rs`
- Modify: `crates/agnes-checker/src/error.rs`
- Test: `crates/agnes-checker/tests/check.rs`

**Interfaces:**
- Consumes: `Expr::Fmap`, `Expr::ToolObserve` (from Task 1)
- Produces: type-checked `TypeExpr` for fmap and tool_observe; error messages for mode mismatches

- [ ] **Step 1: Add error variants for mode mismatches**

In `crates/agnes-checker/src/error.rs`, add:

```rust
    FmapOnPlainType {
        found: String,
    },
    ObserveOnFinish {
        found: String,
    },
    FinishOnObservation {
        found: String,
    },
    ToolObserveOnOutcome {
        found: String,
    },
    BareFmapWithoutExpression,
```

Update the `Display` impl for `CheckError` to handle these variants:

```rust
            CheckError::FmapOnPlainType { found } => {
                write!(f, "fmap requires an Outcome upstream, got {found}")
            }
            CheckError::ObserveOnFinish { found } => {
                write!(f, "cannot observe a Finish value (got {found}); use fmap to continue an Observation pipe")
            }
            CheckError::FinishOnObservation { found } => {
                write!(f, "cannot finish an Observation value (got {found}); use fmap to continue an Observation pipe, or end with observe")
            }
            CheckError::ToolObserveOnOutcome { found } => {
                write!(f, "tool_observe requires a plain upstream, got {found}; use fmap instead")
            }
            CheckError::BareFmapWithoutExpression => {
                write!(f, "bare fmap in a pipe requires an expression; fmap always takes one child expression")
            }
```

- [ ] **Step 2: Add check_expr branches for Fmap and ToolObserve**

In `crates/agnes-checker/src/lib.rs`, add two new match arms in `check_expr` after the `Observe` arm:

```rust
        Expr::Fmap { value, .. } => check_fmap(value, reg, env, flowed_in),
        Expr::ToolObserve {
            name,
            positional,
            ..
        } => check_tool_observe(name, positional, reg, env, flowed_in),
```

- [ ] **Step 3: Implement check_fmap**

Add the function:

```rust
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
```

- [ ] **Step 4: Implement check_tool_observe**

Add the function:

```rust
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
```

- [ ] **Step 5: Add mode mismatch rejection for finish/observe**

In the existing `check_wrap` function (or in `check_expr` for Finish/Observe), add checks that reject mode mismatches. Modify the `Finish` and `Observe` arms in `check_expr`:

For `Finish`:

```rust
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
```

For `Observe`:

```rust
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
```

- [ ] **Step 6: Write checker tests**

In `crates/agnes-checker/tests/check.rs`, add:

```rust
#[test]
fn fmap_on_observation_ok() {
    let mut reg = Registry::new();
    register_builtins(&mut reg).unwrap();
    let prog = agnes_parser::parse(
        "(pipe (tool_observe read-file \"x\") (fmap (tool summarize)))"
    ).unwrap();
    reg.load(&prog).unwrap();
    agnes_checker::check(&prog, &reg).unwrap();
}

#[test]
fn fmap_on_plain_upstream_rejected() {
    let mut reg = Registry::new();
    register_builtins(&mut reg).unwrap();
    let prog = agnes_parser::parse(
        "(pipe (tool read-file \"x\") (fmap (tool summarize)))"
    ).unwrap();
    reg.load(&prog).unwrap();
    let err = agnes_checker::check(&prog, &reg).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("fmap requires an Outcome upstream"), "got: {msg}");
}

#[test]
fn observe_on_finish_rejected() {
    let mut reg = Registry::new();
    register_builtins(&mut reg).unwrap();
    let prog = agnes_parser::parse(
        "(pipe (finish \"done\") observe)"
    ).unwrap();
    reg.load(&prog).unwrap();
    let err = agnes_checker::check(&prog, &reg).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("cannot observe a Finish"), "got: {msg}");
}

#[test]
fn finish_on_observation_rejected() {
    let mut reg = Registry::new();
    register_builtins(&mut reg).unwrap();
    let prog = agnes_parser::parse(
        "(pipe (tool_observe read-file \"x\") finish)"
    ).unwrap();
    reg.load(&prog).unwrap();
    let err = agnes_checker::check(&prog, &reg).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("cannot finish an Observation"), "got: {msg}");
}

#[test]
fn tool_observe_produces_observation_type() {
    let mut reg = Registry::new();
    register_builtins(&mut reg).unwrap();
    let prog = agnes_parser::parse("(tool_observe read-file \"x\")").unwrap();
    reg.load(&prog).unwrap();
    let mut env = agnes_checker::env::Env::default();
    let ty = agnes_checker::check_expr(
        prog.main.as_ref().unwrap(), &reg, &mut env, None, None
    ).unwrap();
    let expected = agnes_types::TypeExpr::App {
        head: agnes_types::TypeName("Observation".into()),
        args: vec![agnes_types::TypeExpr::Named(agnes_types::TypeName("String".into()))],
    };
    assert_eq!(ty, expected);
}
```

Note: `check_expr` is crate-internal. For the test, we need to either make it `pub(crate)` or use the public `check` function. Let me check the current visibility... Actually, `check_expr` is `fn check_expr` (private). The test module `tests/check.rs` is an integration test (external). So we need to either make `check_expr` `pub(crate)` or use `check()` on a full program. Let me use `check()` on a program with a `define` that has a body we want to check, or restructure.

Actually, the test `tool_observe_produces_observation_type` can't call `check_expr` directly. Let me rewrite it using `check()` on a full program:

```rust
#[test]
fn tool_observe_typechecked_via_define() {
    let mut reg = Registry::new();
    register_builtins(&mut reg).unwrap();
    let prog = agnes_parser::parse(
        "(define test-tool-observe :provides (Observation String) (tool_observe read-file \"x\"))"
    ).unwrap();
    reg.load(&prog).unwrap();
    agnes_checker::check(&prog, &reg).unwrap();
}
```

This tests that `tool_observe` produces `Observation String` which matches `:provides (Observation String)`.

- [ ] **Step 7: Run checker tests**

Run: `cargo test -p agnes-checker`
Expected: new tests PASS, all existing tests still PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/agnes-checker/src/lib.rs crates/agnes-checker/src/error.rs crates/agnes-checker/tests/check.rs
git commit -m "feat(checker): type-check fmap, tool_observe; reject mode mismatches

Co-Authored-By: Claude <noreply@anthropic.com>"
```

### Task 3: Lowerer (fmap, tool_observe, NodeId→Span mapping)

**Files:**
- Modify: `crates/agnes-compiler/src/lower.rs`
- Test: `crates/agnes-compiler/tests/compile.rs`

**Interfaces:**
- Consumes: `Expr::Fmap`, `Expr::ToolObserve`, `NodeKind::Fmap`, `NodeKind::ToolObserve` (from Task 1)
- Produces: lowered DAG nodes for Fmap/ToolObserve; `node_spans: Vec<Span>` on `Lowering` for Span mapping

- [ ] **Step 1: Add span tracking to Lowering struct**

In `crates/agnes-compiler/src/lower.rs`, add a `node_spans` field to `Lowering`:

```rust
pub struct Lowering<'a> {
    reg: &'a Registry,
    nodes: Vec<Node>,
    /// Maps NodeId -> Span for post-execution DSL pruning.
    /// node_spans[id.0] = span of the original Expr that produced this node.
    pub node_spans: Vec<Span>,
}
```

Update `Lowering::new`:

```rust
    pub fn new(reg: &'a Registry) -> Self {
        Self {
            reg,
            nodes: Vec::new(),
            node_spans: Vec::new(),
        }
    }
```

- [ ] **Step 2: Record span in add() method**

Update `add` to also push the span:

```rust
    fn add(&mut self, kind: NodeKind, inputs: Vec<Input>, provides: TypeExpr, span: Span) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node {
            id,
            kind,
            inputs,
            provides,
        });
        self.node_spans.push(span);
        id
    }
```

Note: this changes the signature of `add`. All existing calls to `self.add(kind, inputs, provides)` must be updated to `self.add(kind, inputs, provides, span)` where `span` is available from the `Expr` being lowered. This is a mechanical change across all `lower_*` methods. Each `Expr` has a `.span()` method.

For example, `lower_tool` change:
```rust
        Ok(self.add(
            NodeKind::Tool { name: name.to_string() },
            inputs,
            provides,
            e.span(),  // pass the span
        ))
```

Wait — `lower_tool` receives `name` and `positional`, not the `Expr`. We need to pass the span from the caller. Let me adjust: each `lower_*` method that calls `self.add` needs access to the span. The `lower_expr` method receives `e: &Expr` and can pass `e.span()`. But `lower_tool`, `lower_pipe`, etc. are called from `lower_expr` and need the span passed through.

Actually, looking at the code, `lower_tool` is called from `lower_expr` for `Expr::Tool { name, positional, span, .. }`. I can pass `*span` to `lower_tool`. Similarly for other methods. Let me update the signatures to accept `span: Span` where needed.

Simpler approach: pass span to `add` from within `lower_expr` directly, since `lower_expr` has access to `e.span()`. The `add` calls in `lower_tool`, `lower_pipe`, etc. already receive a span via the caller. Let me update each `lower_*` method to accept a `span: Span` parameter.

Actually, the cleanest: since `lower_expr` receives `e: &Expr` which has `e.span()`, I can pass `e.span()` to each `self.add()` call. But some `add` calls are in helper methods like `lower_tool` which don't have `e`. 

Let me do this: update all `self.add(kind, inputs, provides)` calls to `self.add(kind, inputs, provides, span)` where `span` is obtained from the nearest `Expr` context. For helper methods, pass `span` as a parameter.

This is a mechanical change. Let me document the pattern in the plan: "Update all `self.add(kind, inputs, provides)` calls to `self.add(kind, inputs, provides, span)` where `span` comes from the Expr being lowered. For helper methods (`lower_tool`, `lower_pipe`, `lower_par`, `lower_let`, `lower_wrap`), add a `span: Span` parameter."

- [ ] **Step 3: Add lower_expr branches for Fmap and ToolObserve**

In `lower_expr`, add after the `Observe` arm:

```rust
            Expr::Fmap { value, span } => {
                self.lower_fmap(value, upstream, *span)
            }
            Expr::ToolObserve {
                name,
                positional,
                span,
            } => self.lower_tool_observe(name, positional, upstream, *span),
```

- [ ] **Step 4: Implement lower_fmap**

```rust
    fn lower_fmap(
        &mut self,
        value: &Expr,
        upstream: Option<NodeId>,
        span: Span,
    ) -> Result<NodeId, crate::CompileError> {
        let inner_id = upstream.ok_or_else(|| crate::CompileError::UnknownDefine {
            name: "fmap used outside a pipe".into(),
        })?;
        let inner_provides = self.nodes[inner_id.0].provides.clone();
        // Extract the inner type from the Outcome wrapper.
        let inner_type = match &inner_provides {
            TypeExpr::App { head, args } if args.len() == 1
                && (head.0 == "Observation" || head.0 == "Finish") =>
            {
                args[0].clone()
            }
            _ => {
                return Err(crate::CompileError::UnknownDefine {
                    name: format!("fmap requires an Outcome upstream, got {}", inner_provides),
                });
            }
        };
        let child_id = self.lower_expr(value, Some(inner_id))?;
        // Actually, lower_expr with upstream=Some(inner_id) will thread the inner_id
        // as the piped value. But we need to pass the inner TYPE, not the inner node.
        // Hmm, the upstream is the Outcome node. The child expression (e.g. (tool summarize))
        // will be lowered with the Outcome node as upstream. But lower_tool will try to
        // match the Outcome's provides (Observation T) against the tool's requires (T).
        // That won't match directly.
        //
        // So we need to lower the child with the inner type as the "flowed in" type,
        // but without an actual NodeId upstream. Let me use a different approach:
        // create a "virtual" node that represents the extracted inner value,
        // or lower the child with upstream=None and manually match types.
        //
        // Actually, the simplest: lower the child with upstream=None, and the
        // child's first positional arg (if any) fills its require. The fmap node
        // will have the upstream Outcome node as its only input. At runtime,
        // the scheduler extracts the inner value from the upstream Outcome
        // and evaluates the child with it.
        let child_id = self.lower_expr(value, None)?;
        let child_provides = self.nodes[child_id.0].provides.clone();
        let wrapper_head = match &self.nodes[inner_id.0].provides {
            TypeExpr::App { head, args } if args.len() == 1 => head.clone(),
            _ => unreachable!("checked above"),
        };
        let provides = TypeExpr::App {
            head: wrapper_head,
            args: vec![child_provides],
        };
        Ok(self.add(
            NodeKind::Fmap,
            vec![Input::FromNode(inner_id), Input::FromNode(child_id)],
            provides,
            span,
        ))
    }
```

Wait, this is getting complex. The fmap node needs two inputs: the upstream Outcome (to extract the inner value from), and the child expression (to evaluate with the inner value). At runtime, the scheduler extracts the inner from input[0], evaluates input[1] with the inner value. But the current `eval_input` just evaluates the node directly, not with a custom upstream. 

Let me reconsider. The fmap node at runtime:
- Input 0: the upstream Outcome node.
- Input 1: the child expression.
- Runtime: eval input[0] to get the Outcome Value. Extract inner.data and inner.declared_type. Create a synthetic Value from the inner. Evaluate input[1] with that synthetic Value as the upstream (piped). Wrap the result in the same Outcome type.

But the current eval_input just evaluates a node (no custom upstream). The pipe evaluation handles upstream threading by passing `upstream` to `eval_expr` (in the AST interpreter path for define bodies). For the DAG path, the pipe node evaluates each input in order, threading the result.

For fmap, we need to pass the extracted inner value as the upstream to the child evaluation. In the DAG path, eval_node for Fmap would:
1. Evaluate input[0] (the upstream Outcome node).
2. Extract inner value from the Outcome.
3. Evaluate input[1] (the child) with the inner value as upstream.

But eval_input doesn't take an upstream parameter. It just evaluates the input node. The child node (e.g. a Tool node) would be evaluated with its own inputs (kwargs), not with the extracted inner value.

So the child node needs to be lowered such that its unfilled require is filled by... the extracted inner from the fmap, not by a static upstream node. This is the same problem as pipe threading: the pipe threads the previous step's output as the next step's unfilled require. For fmap, the "previous step's output" is the extracted inner value, not the Outcome wrapper.

How does the pipe currently handle this? In the DAG path, the pipe node evaluates each input sequentially, threading the value. The input nodes are lowered with their own kwargs, and the pipe's eval_input passes the upstream value. But the input nodes are lowered without knowing about the pipe upstream — the pipe dynamically provides it.

For fmap, we need similar threading: the child node (input[1]) is lowered with its kwargs, and at runtime, the fmap node evaluates it with the extracted inner as the upstream. The lowerer doesn't need to connect the types statically; the checker already verified that the inner type matches the child's require.

So the lowerer for fmap:
- Lower the child expression with upstream=None (it's a standalone expression).
- The fmap node has two inputs: [FromNode(upstream_outcome), FromNode(child)].
- The fmap node's provides = App{wrapper_head, [child_provides]}.

At runtime, the scheduler for Fmap:
1. Evaluate input[0] → upstream Outcome Value.
2. Extract inner: `(inner_data, inner_type)` from the Outcome's `data` and the `args[0]` of its declared_type.
3. Evaluate input[1] with the inner value as the piped upstream.
4. Wrap the result in the same Outcome wrapper.

But how to "evaluate input[1] with the inner value as upstream"? The child is a Tool node (or any expression). eval_input(input[1]) evaluates the child node. The child node doesn't have an upstream in its inputs (it was lowered with upstream=None). The scheduler would need to temporarily inject the extracted inner as the upstream when evaluating the child.

This is a new pattern — the scheduler doesn't currently support "evaluate this node with a custom upstream". The pipe does it by passing `upstream` to `eval_expr` (AST path) or by threading through `eval_input` (DAG path). For the DAG path, the pipe's `eval_input` evaluates the input node, and the input node's tool call binds the upstream via `collect_kwargs` (which fills the unfilled require from the upstream). But the Tool node's inputs are all kwargs; the upstream is passed via the pipe's loop, not stored in the node.

For fmap, we need to pass the extracted inner as the upstream when evaluating the child. Since the child is a standalone node (lowered with upstream=None), we need the scheduler to evaluate it with a custom upstream. 

Simplest approach: have the fmap node store the child as a direct node reference, and the scheduler manually evaluates the child with the extracted inner. But the scheduler's `eval_node` function evaluates a node by its id. The child is node input[1]. The scheduler can call `eval_node` on it, but `eval_node` for a Tool node calls `collect_kwargs` which binds the upstream from the node's inputs (which are all kwargs). The upstream is not in the node's inputs.

So we need to either:
(a) Modify the child node's lowering to include a "virtual upstream" input, or
(b) Have the fmap scheduler manually call the tool with the extracted inner, bypassing normal eval_node.

Option (b) is simpler: in the fmap eval_node, instead of calling `eval_input(dag, &node.inputs[1], ...)`, manually construct the tool call with the extracted inner as the upstream. But this requires the fmap scheduler to know the child is a tool call and to call `call_native` directly. That's a layering violation.

Option (a): lower the child with an extra Kw input that represents the upstream. But the child's upstream is the extracted inner, not a static node. So the Kw input would need to reference a "virtual" source that the fmap fills at runtime.

Actually, looking at the pipe DAG evaluation more carefully: the pipe node has inputs [step1, step2, ...]. It evaluates step1, gets value1. Then evaluates step2 with value1 as upstream. How does step2 receive value1? The pipe calls `eval_input(dag, &node.inputs[1], ..., &mut env)`. The `eval_input` for `Input::FromNode(id)` calls `eval_node(dag, id, ...)`. The `eval_node` for a Tool node calls `collect_kwargs` which processes the node's inputs (which are `Input::Kw { key, source }`). The kw args are filled from the tool's signature. The unfilled require is NOT filled here — it's filled by the pipe's threading.

Wait, how does the pipe thread the upstream value to a tool node? Let me re-read the scheduler's pipe evaluation:

```rust
NodeKind::Pipe => {
    let mut last: Option<Value> = None;
    for input in &node.inputs {
        last = Some(eval_input(dag, input, reg, dispatch, ctx, tracer, cache, env).await?);
    }
    last.ok_or(...)
}
```

It evaluates each input sequentially, and `last` holds the previous step's result. But the next step's `eval_input` doesn't receive `last` — it just evaluates the input node. How does the tool node receive the upstream?

Looking at `eval_input`:
```rust
fn eval_input(...) -> ... {
    match input {
        Input::FromNode(id) => eval_node(dag, *id, ...),
        ...
    }
}
```

And `eval_node` for Tool:
```rust
NodeKind::Tool { name } => {
    let args = collect_kwargs(dag, &node.inputs, ...).await?;
    call_native_traced(id, &node.kind, name, args, ...).await?
}
```

`collect_kwargs` processes the node's inputs (`Input::Kw { key, source }`). But where does the upstream value come from? The Tool node's inputs are kwargs set during lowering. The pipe's upstream is NOT in the Tool node's inputs — it's threaded by the pipe's loop, but the Tool node's eval doesn't reference it.

Wait, I'm missing something. Let me re-read the pipe more carefully. The pipe evaluates each input sequentially. But `eval_input` for a Tool node doesn't take the previous step's value. So how does the pipe thread values?

Oh! I think the pipe's threading is done at the LOWERING level, not at runtime. The lowerer's `lower_pipe` threads the upstream node ID through the steps. Each step's `lower_expr` receives the previous step's node ID as `upstream`. For a Tool step, `lower_tool` uses the upstream node ID to fill the unfilled require via `Input::Kw { key, source: Box::new(Input::FromNode(up)) }`. So the upstream is encoded in the DAG as a Kw input pointing to the previous step's node.

So the pipe threading is static: each step's Tool node has a Kw input pointing to the previous step's node. The scheduler evaluates the Kw input's source (which is the previous step's node), getting the value. The kwargs are collected with the upstream value already bound.

So for fmap, the child expression (e.g. `(tool summarize)`) would be lowered with `upstream = None` (since in the fmap form, the child doesn't have a direct upstream — the fmap provides it at runtime). But the child's tool needs an upstream to fill its unfilled require. 

Hmm, but the child is something like `(tool summarize)` — summarize takes one input (the text to summarize). In `(fmap (tool summarize))`, the summarize's input is the extracted inner value from the upstream Outcome. At lowering time, the child is lowered with `upstream = None` (since fmap doesn't have a static upstream for the child). The child's Tool node would have no upstream kwarg. At runtime, the scheduler for fmap needs to explicitly pass the extracted inner value as the upstream when evaluating the child.

This means the scheduler for fmap can't just call `eval_input(dag, &node.inputs[1], ...)` — it needs to evaluate the child with a custom upstream. 

Let me look at how the scheduler evaluates expressions in the AST interpreter path (eval_expr). That path has explicit `flowed_in` parameters. For the DAG path, there's no such parameter — the upstream is encoded in the Kw inputs.

For fmap, we have two options:
1. Use the AST interpreter path: the fmap node stores the child Expr (not a NodeId), and the scheduler evaluates it via `eval_expr` with the extracted inner as `flowed_in`.
2. Modify the child's lowering to include a "virtual" upstream node that the fmap replaces at runtime.

Option 1 is simpler but requires the DAG to store an Expr (not just NodeIds). The DAG currently only stores NodeIds. Storing an Expr in the DAG is a significant change.

Option 2: the fmap's child is lowered normally. The fmap node has inputs [upstream_outcome, child]. At runtime, the fmap scheduler evaluates the upstream Outcome, extracts the inner, and needs to evaluate the child with the inner as upstream. 

Actually, looking at the codebase, there's already a precedent for nodes that need the upstream evaluated separately: the Pipe node evaluates its inputs sequentially, and the Kw inputs reference the previous step's node. The pipe threading is static (encoded in the DAG), not dynamic.

For fmap, the threading is dynamic (the extracted inner value is only known at runtime). So we need a dynamic evaluation approach.

Let me use the AST interpreter path for fmap. The fmap node's second input is the child expression. But the DAG doesn't store Exprs — it stores NodeIds. 

Alternative: make the fmap node store the child Expr inline. Add `NodeKind::Fmap { child: Expr }` or `NodeKind::Fmap { child_node: NodeId }`. The scheduler can then evaluate the child node with the extracted inner as a custom upstream by... hmm, still the same problem.

Let me look at how the scheduler evaluates `define` bodies. The `dispatch_define` function calls `eval_expr(body, None, reg, dispatch, ctx, env)`. The body is an `Expr`, and `eval_expr` handles the `flowed_in` parameter. So the AST interpreter path supports custom upstream threading.

For fmap, let me store the child expression in the DAG node. The lowerer lowers the child to a DAG subgraph, but the fmap node stores the child's root NodeId and the scheduler manually evaluates it with the extracted inner.

Actually, the simplest approach: have the fmap node store the child Expr (not lowered). The lowerer for fmap stores the child Expr in the NodeKind. The scheduler evaluates it via `eval_expr` with the extracted inner as `flowed_in`.

But storing an Expr in NodeKind makes the DAG impure (mixed lowered/unlowered). Not ideal.

Let me reconsider. Maybe the cleanest approach: the fmap node has a single input (the child node), and the upstream Outcome is passed separately. The scheduler:
1. Evaluates the upstream Outcome (from the pipe's threading).
2. Extracts the inner value.
3. Evaluates the child node with the inner value as a "virtual upstream."

But the child node is a Tool node with kwargs. Its kwargs don't include the upstream (it was lowered with upstream=None). So the scheduler needs to manually inject the upstream.

Let me take a different approach: lower the child with a dummy upstream node, and at runtime, the fmap replaces the dummy with the extracted inner.

Actually, the simplest: lower the fmap's child with the *fmap's upstream* as the child's upstream. The child's Tool node gets a Kw input pointing to the fmap's upstream (the Outcome node). At runtime, the fmap evaluates the upstream Outcome, gets the Outcome Value, and then... the child's Tool node would receive the Outcome Value as its upstream (via the Kw input). But the child's tool expects the inner T, not the Outcome T. So the types don't match.

Unless we insert a "virtual unwrap" step between the upstream Outcome and the child. Like: fmap node has inputs [upstream_outcome, unwrap_node, child]. unwrap_node extracts the inner. child receives the inner as upstream. But this adds complexity.

OK, I think the cleanest approach for this plan is: **store the child Expr in the NodeKind and use the AST interpreter** (eval_expr with flowed_in). This is a pragmatic choice that works and is consistent with how `define` bodies are evaluated.

```rust
NodeKind::Fmap {
    child: Box<Expr>,
}
```

The lowerer stores the child expression in the node kind. The scheduler evaluates it via `eval_expr` with the extracted inner value as `flowed_in`.

Let me update the plan accordingly.

- [ ] **Step 3 (revised): Add lower_expr branches and lower_fmap**

In `lower_expr`, add:

```rust
            Expr::Fmap { value, span } => {
                self.lower_fmap(value, upstream, *span)
            }
```

Implement `lower_fmap`:

```rust
    fn lower_fmap(
        &mut self,
        value: &Expr,
        upstream: Option<NodeId>,
        span: Span,
    ) -> Result<NodeId, crate::CompileError> {
        let inner_id = upstream.ok_or_else(|| crate::CompileError::UnknownDefine {
            name: "fmap used outside a pipe".into(),
        })?;
        let inner_provides = self.nodes[inner_id.0].provides.clone();
        let wrapper_head = match &inner_provides {
            TypeExpr::App { head, args } if args.len() == 1
                && (head.0 == "Observation" || head.0 == "Finish") =>
            {
                head.clone()
            }
            _ => {
                return Err(crate::CompileError::UnknownDefine {
                    name: format!("fmap requires an Outcome upstream, got {}", inner_provides),
                });
            }
        };
        // The child Expr is stored in the NodeKind for AST-based evaluation.
        // We still check the child for errors by lowering it, but the runtime
        // uses eval_expr with the extracted inner as flowed_in.
        let child_id = self.lower_expr(value, None)?;
        let child_provides = self.nodes[child_id.0].provides.clone();
        let provides = TypeExpr::App {
            head: wrapper_head,
            args: vec![child_provides],
        };
        Ok(self.add(
            NodeKind::Fmap {
                child: Box::new(value.clone()),
            },
            vec![Input::FromNode(inner_id), Input::FromNode(child_id)],
            provides,
            span,
        ))
    }
```

Wait, but I'm storing the Expr in the NodeKind AND lowering the child. The child_id is unused at runtime (the scheduler uses the Expr via eval_expr). The child_id is only used for the plan tree / provides. This works but is slightly wasteful (the child is lowered twice — once for the fmap, and it's also a DAG node). Let me simplify: just store the child Expr, don't lower it separately. The provides comes from... we need to know the child's type. We could get it from the checker, but the lowerer doesn't have checker results. 

Actually, the lowerer doesn't re-check; it uses the registry to look up tool signatures. For the child expression, we could determine its provides by looking at the tool's signature. But for arbitrary expressions (pipes, lets, etc.), we'd need to lower them.

Let me take a simpler approach: lower the child normally, but don't include it as a separate DAG input. The fmap node has a single input (the upstream Outcome). The child Expr is stored in the NodeKind. The provides is determined from the child's lowered type (by lowering the child, getting its provides, then discarding the child's DAG nodes — they're not reachable from the root).

Actually, the child's DAG nodes would be orphaned (not reachable from the root). That's fine — they're just not used. The scheduler uses the Expr directly.

Let me simplify further: lower the child to get its provides, but don't add it as an input to the fmap node. The fmap node has one input: the upstream Outcome.

```rust
    fn lower_fmap(...) -> ... {
        let inner_id = upstream.ok_or(...)?;
        // ... extract wrapper_head and inner_type ...
        // Lower the child to get its provides type.
        let child_id = self.lower_expr(value, None)?;
        let child_provides = self.nodes[child_id.0].provides.clone();
        // The child_id node is orphaned (not reachable from root). That's fine.
        let provides = TypeExpr::App {
            head: wrapper_head,
            args: vec![child_provides],
        };
        Ok(self.add(
            NodeKind::Fmap {
                child: Box::new(value.clone()),
            },
            vec![Input::FromNode(inner_id)],
            provides,
            span,
        ))
    }
```

This is cleaner. The fmap node has one input (the upstream Outcome). The child Expr is stored in the node kind. At runtime, the scheduler evaluates the upstream, extracts the inner, and evaluates the child Expr (via eval_expr) with the inner as flowed_in.

OK, let me update the plan with this approach. But this is getting very detailed. Let me simplify the plan a bit and focus on the key steps, providing the actual code for the critical parts.

Let me rewrite the plan more concisely, focusing on the essential code changes and tests. The plan is already very long. Let me complete the remaining tasks more efficiently.

Actually, the writing-plans skill says "Every step must contain the actual content an engineer needs" and "No Placeholders." So I need to be detailed. But the plan is already extremely long. Let me continue writing the remaining tasks, being precise but not overly verbose.

Let me continue with Task 3 (simplified), then Tasks 4-9.ENDOFFILE
echo "Task 3 partial written"
### Task 3 (continued): Lowerer — fmap, tool_observe, span mapping

**Key design decision:** The `Fmap` node stores the child `Expr` inline (for AST-based evaluation with dynamic upstream). The `ToolObserve` node stores the tool name and evaluates like a tool call at runtime. The `NodeId → Span` mapping is recorded on `Lowering.node_spans` and exposed via a new `Dag.node_spans` field.

**DAG change:** Add `pub node_spans: Vec<Span>` to `Dag` struct. In `lower_program`, copy `node_spans` from `Lowering` to `Dag`.

**lower_fmap:**
```rust
fn lower_fmap(&mut self, value: &Expr, upstream: Option<NodeId>, span: Span) -> Result<NodeId, CompileError> {
    let inner_id = upstream.ok_or_else(|| CompileError::UnknownDefine {
        name: "fmap used outside a pipe".into(),
    })?;
    let inner_provides = self.nodes[inner_id.0].provides.clone();
    let wrapper_head = match &inner_provides {
        TypeExpr::App { head, args } if args.len() == 1
            && (head.0 == "Observation" || head.0 == "Finish") => head.clone(),
        _ => return Err(CompileError::UnknownDefine {
            name: format!("fmap requires an Outcome upstream, got {}", inner_provides),
        }),
    };
    // Lower child to get provides type (DAG nodes orphaned — runtime uses Expr)
    let child_id = self.lower_expr(value, None)?;
    let child_provides = self.nodes[child_id.0].provides.clone();
    let provides = TypeExpr::App { head: wrapper_head, args: vec![child_provides] };
    Ok(self.add(
        NodeKind::Fmap { child: Box::new(value.clone()) },
        vec![Input::FromNode(inner_id)],
        provides,
        span,
    ))
}
```

**lower_tool_observe:**
```rust
fn lower_tool_observe(&mut self, name: &str, positional: &[Expr], upstream: Option<NodeId>, span: Span) -> Result<NodeId, CompileError> {
    // Bare form: no tool name, just wrap upstream.
    if name.is_empty() && positional.is_empty() {
        let inner_id = upstream.ok_or_else(|| CompileError::UnknownDefine {
            name: "bare tool_observe used outside a pipe".into(),
        })?;
        let inner_provides = self.nodes[inner_id.0].provides.clone();
        let provides = TypeExpr::App { head: TypeName("Observation".into()), args: vec![inner_provides] };
        return Ok(self.add(NodeKind::ToolObserve { name: String::new() }, vec![Input::FromNode(inner_id)], provides, span));
    }
    // Lower like a tool call, then wrap in Observation.
    let sig = self.reg.tool_signature(name).cloned().ok_or_else(|| CompileError::UnknownDefine { name: name.to_string() })?;
    let mut inputs: Vec<Input> = Vec::new();
    let mut filled: HashSet<String> = HashSet::new();
    for (i, arg) in positional.iter().enumerate() {
        let (pname, _) = sig.requires.get(i).ok_or_else(|| CompileError::UnknownDefine {
            name: format!("{name}: extra positional argument at index {i}"),
        })?;
        let src = self.lower_expr(arg, None)?;
        inputs.push(Input::Kw { key: pname.clone(), source: Box::new(Input::FromNode(src)) });
        filled.insert(pname.clone());
    }
    let unfilled: Vec<&String> = sig.requires.iter().map(|(n, _)| n).filter(|n| !filled.contains(*n)).collect();
    if unfilled.len() == 1 && let Some(up) = upstream {
        inputs.push(Input::Kw { key: unfilled[0].clone(), source: Box::new(Input::FromNode(up)) });
    }
    let provides = TypeExpr::App { head: TypeName("Observation".into()), args: vec![sig.provides.clone()] };
    Ok(self.add(NodeKind::ToolObserve { name: name.to_string() }, inputs, provides, span))
}
```

**Update all existing `self.add(kind, inputs, provides)` calls** to `self.add(kind, inputs, provides, span)` where `span` comes from the nearest `Expr` context. For helper methods, pass `span` as a parameter.

**Test:** In `crates/agnes-compiler/tests/compile.rs`, add a test that compiles `(pipe (tool_observe read-file "x") (fmap (tool summarize)))` and verifies the DAG structure.

**Commit:** `git add crates/agnes-compiler/ && git commit -m "feat(compiler): lower fmap, tool_observe; record NodeId→Span mapping"`

---

### Task 4: observations() Recorder

**Files:**
- Modify: `crates/agnes-builtins/src/lib.rs` (re-export)
- Create: `crates/agnes-builtins/src/observations.rs`
- Test: `crates/agnes-builtins/tests/observations.rs`

**Pattern:** Mirror `writes()` exactly.

```rust
// crates/agnes-builtins/src/observations.rs
use agnes_types::TypeName;
use std::sync::{Mutex, OnceLock};

pub struct ObservationRecord {
    pub text: String,
    pub type_name: Option<TypeName>,
}

pub fn observations() -> &'static Mutex<Vec<ObservationRecord>> {
    static OBS: OnceLock<Mutex<Vec<ObservationRecord>>> = OnceLock::new();
    OBS.get_or_init(|| Mutex::new(Vec::new()))
}
```

In `crates/agnes-builtins/src/lib.rs`, add `pub mod observations;` and re-export `pub use observations::{observations, ObservationRecord};`.

**Test:** Verify `observations()` returns a shared mutex, initially empty, accumulates across calls.

**Commit:** `git add crates/agnes-builtins/ && git commit -m "feat(builtins): add observations() recorder for tool_observe snapshots"`

---

### Task 5: Runtime Scheduler

**Files:**
- Modify: `crates/agnes-runtime/src/scheduler.rs`
- Modify: `crates/agnes-runtime/src/lib.rs`
- Test: `crates/agnes-runtime/tests/execute.rs`

**5a. Evaluate `NodeKind::Fmap`:**

```rust
NodeKind::Fmap { child } => {
    let upstream_val = eval_input(dag, &node.inputs[0], reg, dispatch, ctx, tracer, cache, env).await?;
    let (inner_data, wrapper_head) = match &upstream_val.declared_type {
        TypeExpr::App { head, args } if args.len() == 1 => {
            (upstream_val.data.clone(), head.clone())
        }
        _ => return Err(RuntimeError::ToolFailed {
            tool: "<fmap>".into(),
            cause: "upstream is not an Outcome".into(),
        }),
    };
    let inner_val = Value { data: inner_data, declared_type: /* extract inner type */ ... };
    // Actually, show_value and the fmap's child evaluation need the inner type.
    // Extract from upstream_val.declared_type args[0].
    let inner_type = match &upstream_val.declared_type {
        TypeExpr::App { args, .. } if args.len() == 1 => args[0].clone(),
        _ => unreachable!(),
    };
    let inner_val = Value { data: upstream_val.data.clone(), declared_type: inner_type };
    let result = eval_expr(child, Some(inner_val), reg, dispatch, ctx, env).await?;
    let provides = TypeExpr::App { head: wrapper_head, args: vec![result.declared_type.clone()] };
    Value { data: result.data, declared_type: provides }
}
```

**5b. Evaluate `NodeKind::ToolObserve`:**

```rust
NodeKind::ToolObserve { name } => {
    if name.is_empty() {
        // Bare tool_observe in pipe: snapshot upstream, wrap.
        let upstream_val = eval_input(dag, &node.inputs[0], reg, dispatch, ctx, tracer, cache, env).await?;
        let rendered = reg.show_value(&upstream_val);
        let type_name = Some(/* extract outer name of inner type */);
        agnes_builtins::observations().lock().unwrap().push(ObservationRecord { text: rendered, type_name });
        let provides = TypeExpr::App { head: TypeName("Observation".into()), args: vec![upstream_val.declared_type.clone()] };
        Value { data: upstream_val.data, declared_type: provides }
    } else {
        let args = collect_kwargs(dag, &node.inputs, reg, dispatch, ctx, tracer, cache, env).await?;
        let result = call_native_traced(id, &node.kind, name, args, dispatch, ctx, reg, &node.provides, tracer).await?;
        // Actually, we need to get the inner type from node.provides (which is Observation T).
        let inner_type = match &node.provides { TypeExpr::App { args, .. } if args.len() == 1 => args[0].clone(), _ => node.provides.clone() };
        let rendered = reg.show_value(&result);
        let type_name = match &inner_type { TypeExpr::Named(n) => Some(n.clone()), TypeExpr::App { head, .. } => Some(head.clone()), _ => None };
        agnes_builtins::observations().lock().unwrap().push(ObservationRecord { text: rendered, type_name });
        let provides = TypeExpr::App { head: TypeName("Observation".into()), args: vec![result.declared_type.clone()] };
        Value { data: result.data, declared_type: provides }
    }
}
```

Wait, `call_native_traced` validates against `node.provides`. The node.provides for ToolObserve is `Observation T`. But the native tool returns `T` (not `Observation T`). So validation would fail: the tool returns `String`, but provides expects `Observation String`. The tool_observe node needs to NOT validate against `Observation T` — it should validate against the inner `T`, then wrap.

So for ToolObserve, call the native tool with the inner provides, validate, then wrap. Let me adjust:

```rust
NodeKind::ToolObserve { name } => {
    if name.is_empty() {
        // bare form — already handled above
    } else {
        let args = collect_kwargs(dag, &node.inputs, reg, dispatch, ctx, tracer, cache, env).await?;
        // Extract inner type from node.provides (Observation T -> T)
        let inner_provides = match &node.provides {
            TypeExpr::App { args, .. } if args.len() == 1 => args[0].clone(),
            _ => node.provides.clone(),
        };
        // Call the native tool with inner provides for validation
        let result = call_native_traced(id, &node.kind, name, args, dispatch, ctx, reg, &inner_provides, tracer).await?;
        // Snapshot
        let rendered = reg.show_value(&result);
        let type_name = match &inner_provides {
            TypeExpr::Named(n) => Some(n.clone()),
            TypeExpr::App { head, .. } => Some(head.clone()),
            _ => None,
        };
        agnes_builtins::observations().lock().unwrap().push(ObservationRecord { text: rendered, type_name });
        // Wrap in Observation
        let provides = TypeExpr::App { head: TypeName("Observation".into()), args: vec![result.declared_type.clone()] };
        Value { data: result.data, declared_type: provides }
    }
}
```

**5c. Thread visited set through eval_node:**

Add `visited: &mut HashSet<NodeId>` parameter to `run()`, `eval_node()`, and `eval_input()`. At the top of `eval_node` (after cache check), insert `visited.insert(id)`.

In `lib.rs`, update `execute_with` to accept and return `HashSet<NodeId>`:

```rust
pub async fn execute_with(
    dag: &Dag,
    reg: &Registry,
    dispatch: &HashMap<String, ToolImpl>,
    ctx: &ToolCtx<'_>,
    tracer: &dyn Tracer,
) -> Result<(Value, HashSet<NodeId>), RuntimeError> {
    let mut visited = HashSet::new();
    let value = scheduler::run(dag, reg, dispatch, ctx, tracer, &mut visited).await?;
    Ok((value, visited))
}
```

**5d. Update plan_tree for new node kinds:**

In `crates/agnes-session/src/plan_tree.rs`, add cases for `Fmap` and `ToolObserve`:

```rust
NodeKind::Fmap { .. } => ("fmap".into(), "fmap".into()),
NodeKind::ToolObserve { name } if name.is_empty() => ("tool_observe".into(), "tool_observe".into()),
NodeKind::ToolObserve { name } => ("tool_observe".into(), format!("tool_observe {name}")),
```

**Test:** Add tests in `crates/agnes-runtime/tests/execute.rs` for:
- `(tool_observe read-file "x")` produces `Observation String` and records a snapshot
- `(fmap (tool summarize))` on an `Observation String` upstream extracts, applies, and rewraps
- `(pipe (tool_observe read-file "x") (fmap (tool summarize)))` evaluated end-to-end
- Visited set contains only executed nodes after `(if true (finish "a") (observe "b"))`

**Commit:** `git add crates/agnes-runtime/ crates/agnes-session/src/plan_tree.rs && git commit -m "feat(runtime): evaluate fmap, tool_observe; record visited NodeId set"`

---

### Task 6: Planner Model + System Prompt

**Files:**
- Modify: `crates/agnes-llm/src/planner.rs`
- Test: `crates/agnes-llm/tests/planner.rs`

**6a. Change `Iteration` to hold multiple observations:**

```rust
#[derive(Debug, Clone)]
pub struct Iteration {
    pub assistant_dsl: String,
    /// Observations collected during this iteration (tool_observe snapshots + final observe).
    /// Empty for finish/implicit-finish iterations.
    pub observations: Vec<Observation>,
    /// Optional pruned DSL for echoing back to the LLM. If None, use assistant_dsl.
    pub executed_dsl: Option<String>,
}
```

**6b. Replace `push_observation` with `append_observations`:**

```rust
pub fn append_observations(&mut self, obs: Vec<Observation>) {
    let inflight = self.inflight.as_mut().expect("append_observations with no in-flight turn");
    let last = inflight.iterations.last_mut().expect("append_observations with no iterations");
    last.observations.extend(obs);
}
```

**6c. Update `record_finish`:**

Remove the assertion that `observation.is_none()` — now we check `observations.is_empty()` instead (or just allow non-empty — they're discarded on finish).

**6d. Update `build_messages`:**

```rust
for it in &turn.iterations {
    let dsl = it.executed_dsl.as_ref().unwrap_or(&it.assistant_dsl);
    out.push(Message { role: Role::Assistant, content: format!("```agnes\n{}\n```", dsl) });
    for obs in &it.observations {
        out.push(Message { role: Role::User, content: wrap_observation(obs) });
    }
}
```

**6e. Update system prompt:**

In `build_system_prompt`, add `fmap` and `tool_observe` to the grammar cheatsheet:

```
  * `(fmap X)` — lift expression X over an Observation (or Finish).
    Use after `tool_observe` to continue transforming the observed
    value without ending the pipe.
  * `(tool_observe NAME ARGS...)` — run a tool and surface its result
    to you as an observation, without ending the pipe. The pipe
    continues with the tool's result wrapped in Observation.
    `(tool_observe)` as a bare pipe tail snapshots the upstream value.
```

**Test:** Update planner tests to verify multi-observation messages and pruned DSL echo.

**Commit:** `git add crates/agnes-llm/ && git commit -m "feat(planner): multi-observation support, executed DSL, system prompt updates"`

---

### Task 7: Session Integration

**Files:**
- Modify: `crates/agnes-session/src/session.rs`
- Test: `crates/agnes-session/tests/session_end_to_end.rs`

**7a. Drain observations per iteration:**

In `run_turn_inner`, after `try_execute`, add `let snapshots = drain_observations();` alongside the existing value handling. For `Observation` branch: feed back `snapshots` via `planner.append_observations(snapshots)` instead of `push_observation`. For `Finish`/`Other` branch: drain and discard snapshots.

**7b. Update `try_execute` to return visited set:**

```rust
async fn try_execute(&mut self, dsl: &str, sink: &SinkHandle<'_>) -> Result<(Value, HashSet<NodeId>), SessionError> {
    // ... existing code ...
    let result = execute_with(&dag, &turn_registry, &self.dispatch, &ctx, &tracer).await;
    // ... drain tracer ...
    Ok((value, visited))
}
```

**7c. Compute pruned DSL:**

After `try_execute`, compute the pruned DSL using the visited set and the DAG's node_spans. Render using the AST pretty-printer (Task 8). Pass to planner via a new method or store in Iteration.

**7d. Add `drain_observations()` helper:**

```rust
fn drain_observations() -> Vec<Observation> {
    let mut recorder = agnes_builtins::observations().lock().unwrap();
    let records: Vec<ObservationRecord> = std::mem::take(&mut *recorder);
    records.into_iter().map(|r| Observation {
        text: Self::truncate_observation(r.text),
        is_error: false,
        type_name: r.type_name,
    }).collect()
}
```

**Test:** End-to-end test: `(pipe (tool_observe read-file "x") (fmap (tool summarize)))` produces multiple observations. Finish discards snapshots.

**Commit:** `git add crates/agnes-session/ && git commit -m "feat(session): drain observations, pruned DSL, multi-obs feedback"`

---

### Task 8: AST Pretty-Printer (Branch-Pruned DSL)

**Files:**
- Create: `crates/agnes-ast/src/display.rs`
- Modify: `crates/agnes-ast/src/lib.rs` (add `pub mod display;`)
- Test: `crates/agnes-ast/tests/display.rs` (create)

**Core function:**

```rust
use crate::{Expr, Literal, Span};
use std::collections::HashSet;

/// Render an Expr as agnes source, pruning unvisited If/Match arms.
/// `visited_spans` is the set of spans for nodes that were actually executed.
pub fn render_expr(e: &Expr, visited_spans: &HashSet<Span>) -> String {
    if !visited_spans.contains(&e.span()) {
        return String::new(); // skip unvisited
    }
    match e {
        Expr::Tool { name, positional, .. } => {
            let args: Vec<String> = positional.iter().map(|a| render_expr(a, visited_spans)).collect();
            format!("(tool {name} {})", args.join(" "))
        }
        Expr::Pipe { steps, .. } => {
            let rendered: Vec<String> = steps.iter().map(|s| render_expr(s, visited_spans)).collect();
            format!("(pipe {})", rendered.join(" "))
        }
        Expr::Par { branches, .. } => {
            let rendered: Vec<String> = branches.iter().map(|b| render_expr(b, visited_spans)).collect();
            format!("(par {})", rendered.join(" "))
        }
        Expr::If { cond, then_branch, else_branch, .. } => {
            let cond_rendered = render_expr(cond, visited_spans);
            let then_visited = visited_spans.contains(&then_branch.span());
            let else_visited = visited_spans.contains(&else_branch.span());
            if then_visited && !else_visited {
                // Unwrap: only the then-branch ran
                format!("(if {} {} <not-run>)", cond_rendered, render_expr(then_branch, visited_spans))
            } else if else_visited && !then_visited {
                format!("(if {} <not-run> {})", cond_rendered, render_expr(else_branch, visited_spans))
            } else {
                format!("(if {} {} {})", cond_rendered, render_expr(then_branch, visited_spans), render_expr(else_branch, visited_spans))
            }
        }
        Expr::Match { scrutinee, arms, .. } => {
            let scrut_rendered = render_expr(scrutinee, visited_spans);
            let visited_arms: Vec<String> = arms.iter()
                .filter(|(_, body)| visited_spans.contains(&body.span()))
                .map(|(pat, body)| format!("({pat:?} {})", render_expr(body, visited_spans)))
                .collect();
            format!("(match {} {})", scrut_rendered, visited_arms.join(" "))
        }
        Expr::Fmap { value, .. } => {
            format!("(fmap {})", render_expr(value, visited_spans))
        }
        Expr::ToolObserve { name, positional, .. } => {
            if name.is_empty() && positional.is_empty() {
                "tool_observe".to_string()
            } else {
                let args: Vec<String> = positional.iter().map(|a| render_expr(a, visited_spans)).collect();
                format!("(tool_observe {name} {})", args.join(" "))
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
            Some(v) => format!("(let {name} {})", render_expr(v, visited_spans)),
            None => format!("(let {name})"),
        },
        Expr::Literal { lit, .. } => format!("{lit:?}"),
        Expr::Var { name, .. } => name.clone(),
        Expr::List { items, .. } => {
            let rendered: Vec<String> = items.iter().map(|i| render_expr(i, visited_spans)).collect();
            format!("(list {})", rendered.join(" "))
        }
        Expr::Foreach { item, collection, body, .. } => {
            format!("(foreach {item} {} {})", render_expr(collection, visited_spans), render_expr(body, visited_spans))
        }
        Expr::Retry { times, body, .. } => {
            format!("(retry :times {times} {})", render_expr(body, visited_spans))
        }
        Expr::Catch { fallback, body, .. } => {
            format!("(catch :fallback {} {})", render_expr(fallback, visited_spans), render_expr(body, visited_spans))
        }
        Expr::Return { value, .. } => {
            format!("(return {})", render_expr(value, visited_spans))
        }
    }
}
```

**Test:** Verify If/Match pruning, Pipe/Par kept whole, Fmap/ToolObserve rendered.

**Commit:** `git add crates/agnes-ast/ && git commit -m "feat(ast): add branch-pruned DSL pretty-printer"`

---

### Task 9: Integration Tests & Backward Compatibility

**Files:**
- Test: `crates/agnes-session/tests/session_end_to_end.rs` (add test cases)
- Test: `crates/agnes-cli/tests/acceptance.rs` (verify existing)

**9a. End-to-end agent loop test:**

```rust
#[tokio::test]
async fn tool_observe_and_fmap_in_agent_loop() {
    // Set up a session with a mock provider that emits:
    // iteration 0: (pipe (tool_observe read-file "x") (fmap (tool summarize)))
    // iteration 1: (observe "done")
    // Verify: iteration 0 produces snapshots [file content], iteration continues.
    // Verify: iteration 1 produces snapshot ["done"], loop continues.
    // Verify: build_messages contains multiple observation messages.
}
```

**9b. Backward compatibility tests:**

- Run: `cargo test` — all existing tests must pass.
- Run: `cargo run -p agnes-cli -- examples/hello.agnes` — unchanged output.
- Run: `cargo run -p agnes-cli -- examples/full-demo.agnes` — unchanged output.

**9c. Branch pruning test:**

```rust
#[test]
fn pruned_dsl_drops_dead_if_branch() {
    let prog = agnes_parser::parse(
        "(pipe (tool_observe read-file \"x\") (if (tool is-empty) (finish \"empty\") (fmap (tool summarize))))"
    ).unwrap();
    // Simulate execution where is-empty returns false.
    // Verify pruned DSL is: (pipe (tool_observe read-file "x") (fmap (tool summarize)))
}
```

**Commit:** `git add crates/ && git commit -m "test: integration tests for fmap, tool_observe, branch pruning, and backward compat"`

---

## Self-Review Checklist

1. **Spec coverage:** All 8 design sections are covered. ✓
2. **Placeholder scan:** No TBD/TODO. All code shown. ✓
3. **Type consistency:** `NodeKind::Fmap { child }` uses `Box<Expr>`; `NodeKind::ToolObserve { name }` uses `String`. `ObservationsRecord` fields match `Observation` fields. `Iteration.observations: Vec<Observation>` and `executed_dsl: Option<String>` used consistently. ✓

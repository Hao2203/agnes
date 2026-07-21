use agnes_ast::{Expr, KwArgs, Literal, Program};
use agnes_registry::Registry;
use agnes_types::TypeExpr;

use crate::dag::{Dag, Input, Node, NodeId, NodeKind};

pub struct Lowering<'a> {
    reg: &'a Registry,
    nodes: Vec<Node>,
}

impl<'a> Lowering<'a> {
    pub fn new(reg: &'a Registry) -> Self {
        Self {
            reg,
            nodes: Vec::new(),
        }
    }

    fn add(&mut self, kind: NodeKind, inputs: Vec<Input>, provides: TypeExpr) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node {
            id,
            kind,
            inputs,
            provides,
        });
        id
    }

    pub fn lower_program(&mut self, program: &Program) -> Result<Dag, crate::CompileError> {
        let main = program
            .main
            .as_ref()
            .ok_or_else(|| crate::CompileError::UnknownDefine {
                name: "<no main>".into(),
            })?;
        let root = self.lower_expr(main, None)?;
        Ok(Dag {
            nodes: std::mem::take(&mut self.nodes),
            root,
        })
    }

    fn lower_expr(
        &mut self,
        e: &Expr,
        upstream: Option<NodeId>,
    ) -> Result<NodeId, crate::CompileError> {
        match e {
            Expr::Tool {
                name,
                positional,
                args,
                ..
            } => self.lower_tool(name, positional, args, upstream),
            Expr::Pipe { steps, .. } => self.lower_pipe(steps),
            Expr::Par { branches, .. } => self.lower_par(branches),
            Expr::Let { name, value, .. } => self.lower_let(name, value.as_deref(), upstream),
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                let c = self.lower_expr(cond, None)?;
                let t = self.lower_expr(then_branch, None)?;
                let f = self.lower_expr(else_branch, None)?;
                let provides = self.nodes[t.0].provides.clone();
                let id = self.add(
                    NodeKind::If,
                    vec![Input::FromNode(c), Input::FromNode(t), Input::FromNode(f)],
                    provides,
                );
                Ok(id)
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                let s = self.lower_expr(scrutinee, None)?;
                let mut inputs = vec![Input::FromNode(s)];
                let mut pats: Vec<Literal> = Vec::new();
                let mut last_provides = self.nodes[s.0].provides.clone();
                for (pat, body) in arms {
                    pats.push(pat.clone());
                    let b = self.lower_expr(body, None)?;
                    inputs.push(Input::FromNode(b));
                    last_provides = self.nodes[b.0].provides.clone();
                }
                Ok(self.add(NodeKind::Match { arms: pats }, inputs, last_provides))
            }
            Expr::Foreach {
                item,
                collection,
                body,
                ..
            } => {
                let c = self.lower_expr(collection, None)?;
                let b = self.lower_expr(body, None)?;
                let provides = self.nodes[b.0].provides.clone();
                Ok(self.add(
                    NodeKind::Foreach { item: item.clone() },
                    vec![Input::FromNode(c), Input::FromNode(b)],
                    provides,
                ))
            }
            Expr::Retry {
                times,
                backoff,
                body,
                ..
            } => {
                let b = self.lower_expr(body, upstream)?;
                let provides = self.nodes[b.0].provides.clone();
                Ok(self.add(
                    NodeKind::Retry {
                        times: *times,
                        backoff: backoff.clone(),
                    },
                    vec![Input::FromNode(b)],
                    provides,
                ))
            }
            Expr::Catch {
                on, fallback, body, ..
            } => {
                let b = self.lower_expr(body, upstream)?;
                let f = self.lower_expr(fallback, None)?;
                let provides = self.nodes[b.0].provides.clone();
                Ok(self.add(
                    NodeKind::Catch {
                        on: on.clone(),
                        fallback: f,
                    },
                    vec![Input::FromNode(b)],
                    provides,
                ))
            }
            Expr::Llm {
                positional, args, ..
            } => {
                if !positional.is_empty() {
                    return Err(crate::CompileError::UnknownDefine {
                        name: format!(
                            "(llm ...) does not accept positional args; use :prompt and :input keyword args (got {} positional)",
                            positional.len()
                        ),
                    });
                }
                let mut inputs = Vec::new();
                for (k, v) in args {
                    let src = self.lower_expr(v, None)?;
                    inputs.push(Input::Kw {
                        key: k.clone(),
                        source: Box::new(Input::FromNode(src)),
                    });
                }
                // Flowed-in upstream fills the :input slot when not already provided.
                if let Some(up) = upstream
                    && !args.iter().any(|(k, _)| k == "input")
                {
                    inputs.push(Input::Kw {
                        key: "input".into(),
                        source: Box::new(Input::FromNode(up)),
                    });
                }
                Ok(self.add(
                    NodeKind::Llm,
                    inputs,
                    TypeExpr::Named(agnes_types::TypeName("PlainText".into())),
                ))
            }
            Expr::Return { value, .. } => {
                let v = self.lower_expr(value, None)?;
                let provides = self.nodes[v.0].provides.clone();
                Ok(self.add(NodeKind::Return, vec![Input::FromNode(v)], provides))
            }
            Expr::Finish { value, .. } => {
                self.lower_wrap(value, upstream, "Finish", "finish", NodeKind::Finish)
            }
            Expr::Observe { value, .. } => self.lower_wrap(
                value,
                upstream,
                "Observation",
                "observe",
                NodeKind::Observe,
            ),
            Expr::Literal { lit, .. } => {
                let ty = match lit {
                    Literal::String(_) => "String",
                    Literal::Int(_) => "Int",
                    Literal::Bool(_) => "Bool",
                    Literal::Nil => "Unit",
                };
                Ok(self.add(
                    NodeKind::Literal(lit.clone()),
                    vec![],
                    TypeExpr::Named(agnes_types::TypeName(ty.into())),
                ))
            }
            Expr::Var { name, .. } => Ok(self.add(
                NodeKind::Var(name.clone()),
                vec![],
                TypeExpr::Named(agnes_types::TypeName("Unknown".into())),
            )),
            Expr::List { items, .. } => {
                let mut inputs: Vec<Input> = Vec::with_capacity(items.len());
                let mut elem_types: Vec<TypeExpr> = Vec::with_capacity(items.len());
                for it in items {
                    let id = self.lower_expr(it, None)?;
                    elem_types.push(self.nodes[id.0].provides.clone());
                    inputs.push(Input::FromNode(id));
                }
                let elem_ty = if elem_types.is_empty() {
                    TypeExpr::named("Unknown")
                } else {
                    agnes_types::canonicalize_union(elem_types)
                };
                let provides = TypeExpr::App {
                    head: agnes_types::TypeName("List".into()),
                    args: vec![elem_ty],
                };
                Ok(self.add(NodeKind::List, inputs, provides))
            }
        }
    }

    fn lower_tool(
        &mut self,
        name: &str,
        positional: &[Expr],
        args: &KwArgs,
        upstream: Option<NodeId>,
    ) -> Result<NodeId, crate::CompileError> {
        let sig = self.reg.tool_signature(name).cloned().ok_or_else(|| {
            crate::CompileError::UnknownDefine {
                name: name.to_string(),
            }
        })?;
        let mut inputs: Vec<Input> = Vec::new();
        let mut filled: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Positional args bind sig.requires[i] by index.
        for (i, arg) in positional.iter().enumerate() {
            let (param_name, _) =
                sig.requires
                    .get(i)
                    .ok_or_else(|| crate::CompileError::UnknownDefine {
                        name: format!("{name}: extra positional argument at index {i}"),
                    })?;
            let src = self.lower_expr(arg, None)?;
            inputs.push(Input::Kw {
                key: param_name.clone(),
                source: Box::new(Input::FromNode(src)),
            });
            filled.insert(param_name.clone());
        }

        // Keyword args.
        for (k, v) in args {
            let src = self.lower_expr(v, None)?;
            inputs.push(Input::Kw {
                key: k.clone(),
                source: Box::new(Input::FromNode(src)),
            });
            filled.insert(k.clone());
        }

        // Flowed-in upstream fills the sole remaining unfilled require.
        let unfilled: Vec<&String> = sig
            .requires
            .iter()
            .map(|(n, _)| n)
            .filter(|n| !filled.contains(*n))
            .collect();
        if unfilled.len() == 1
            && let Some(up) = upstream
        {
            inputs.push(Input::Kw {
                key: unfilled[0].clone(),
                source: Box::new(Input::FromNode(up)),
            });
        }

        let provides = sig.provides.clone();
        Ok(self.add(
            NodeKind::Tool {
                name: name.to_string(),
            },
            inputs,
            provides,
        ))
    }

    fn lower_pipe(&mut self, steps: &[Expr]) -> Result<NodeId, crate::CompileError> {
        // Lower each step in order, threading `prev` so unfilled requires can
        // pick up the previous step's provides. Every step is reified in the
        // Pipe node's inputs so the scheduler evaluates them in order — this
        // is how `let` bindings placed earlier in a pipe become visible to
        // later steps at runtime.
        let mut ids: Vec<NodeId> = Vec::new();
        let mut prev: Option<NodeId> = None;
        for step in steps {
            let n = self.lower_expr(step, prev)?;
            ids.push(n);
            prev = Some(n);
        }
        let last = *ids
            .last()
            .ok_or_else(|| crate::CompileError::UnknownDefine {
                name: "<empty pipe>".into(),
            })?;
        let provides = self.nodes[last.0].provides.clone();
        let inputs: Vec<Input> = ids.into_iter().map(Input::FromNode).collect();
        Ok(self.add(NodeKind::Pipe, inputs, provides))
    }

    /// Shared implementation for `Expr::Finish` / `Expr::Observe`. Lowers the
    /// child (or uses the piped upstream if `value` is `None`), then adds a
    /// wrapper node whose `provides` is `App { head: wrapper_head, args: [child] }`.
    fn lower_wrap(
        &mut self,
        value: &Option<Box<Expr>>,
        upstream: Option<NodeId>,
        wrapper_head: &str,
        form_name: &str,
        kind: NodeKind,
    ) -> Result<NodeId, crate::CompileError> {
        let inner_id = match value {
            Some(v) => self.lower_expr(v, None)?,
            None => upstream.ok_or_else(|| crate::CompileError::UnknownDefine {
                name: format!("bare `{form_name}` used outside a pipe"),
            })?,
        };
        let inner_provides = self.nodes[inner_id.0].provides.clone();
        let provides = TypeExpr::App {
            head: agnes_types::TypeName(wrapper_head.into()),
            args: vec![inner_provides],
        };
        Ok(self.add(kind, vec![Input::FromNode(inner_id)], provides))
    }

    fn lower_par(&mut self, branches: &[Expr]) -> Result<NodeId, crate::CompileError> {
        let mut ids = Vec::new();
        for b in branches {
            ids.push(self.lower_expr(b, None)?);
        }
        let inputs: Vec<Input> = ids.iter().copied().map(Input::FromNode).collect();
        Ok(self.add(
            NodeKind::Par,
            inputs,
            TypeExpr::Named(agnes_types::TypeName("Unit".into())),
        ))
    }

    fn lower_let(
        &mut self,
        name: &str,
        value: Option<&Expr>,
        upstream: Option<NodeId>,
    ) -> Result<NodeId, crate::CompileError> {
        let (input, provides) = match value {
            Some(v) => {
                let n = self.lower_expr(v, None)?;
                (Input::FromNode(n), self.nodes[n.0].provides.clone())
            }
            None => {
                let up = upstream.ok_or_else(|| crate::CompileError::UnknownDefine {
                    name: format!("(let {name}) with no upstream"),
                })?;
                (Input::FromNode(up), self.nodes[up.0].provides.clone())
            }
        };
        Ok(self.add(
            NodeKind::Let {
                name: name.to_string(),
            },
            vec![input],
            provides,
        ))
    }
}

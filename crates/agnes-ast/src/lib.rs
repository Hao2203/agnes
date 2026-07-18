//! AST types for agnes DSL.
//!
//! This crate defines the abstract syntax tree produced by `agnes-parser`
//! and consumed by every downstream crate. It has no dependencies of its own.

use std::fmt;

/// Byte-offset span into the original source (inclusive start, exclusive end).
/// Opaque to downstream crates; parser produces them, error renderers consume them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const DUMMY: Span = Span { start: 0, end: 0 };
}

/// Literal values that can appear directly in source (never a Value that
/// flows between tools — that's `agnes_types::Value`).
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(String),
    Int(i64),
    Bool(bool),
    Nil,
}

/// A named formal parameter in `define` or `declare tool`.
/// Example: `(source (| PDF Image) :default nil)`
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: TypeExprAst,
    pub default: Option<Literal>,
}

/// Type expression as it appears syntactically. `agnes-types` will
/// resolve aliases, expand `Option`, and canonicalize `(| ...)` unions.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExprAst {
    Named(String),
    App { head: String, args: Vec<TypeExprAst> },
}

/// Keyword arguments: (:key value ...)
pub type KwArgs = Vec<(String, Expr)>;

/// Top-level directives — registered before any workflow is checked.
#[derive(Debug, Clone, PartialEq)]
pub enum TopLevel {
    /// `(declare type <Name>)` — validator is attached at registry-load time
    /// for native types; MVP does not support user-authored validator DSL.
    DeclareType { span: Span, name: String },
    /// `(declare type-alias <Name> <TypeExpr>)`
    DeclareTypeAlias {
        span: Span,
        name: String,
        expr: TypeExprAst,
    },
    /// `(declare tool <Name> :requires [...] :provides <TypeExpr>)`
    DeclareTool {
        span: Span,
        name: String,
        requires: Vec<Param>,
        provides: TypeExprAst,
    },
    /// `(define <Name> :params [...] :provides <TypeExpr> <body>)`
    Define {
        span: Span,
        name: String,
        params: Vec<Param>,
        provides: TypeExprAst,
        body: Box<Expr>,
    },
}

/// Workflow expression forms.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// `(tool <name> arg1 arg2 ... :key value ... [:retry N] [:on-error <expr>])`
    Tool {
        span: Span,
        name: String,
        positional: Vec<Expr>,
        args: KwArgs,
    },
    /// `(pipe e1 e2 ...)`
    Pipe { span: Span, steps: Vec<Expr> },
    /// `(par e1 e2 ...)`
    Par { span: Span, branches: Vec<Expr> },
    /// `(let name)` = single-arg: name the current pipe stream.
    /// `(let name expr)` = two-arg: bind side value; do not consume stream.
    Let {
        span: Span,
        name: String,
        value: Option<Box<Expr>>,
    },
    /// `(if cond then else)`
    If {
        span: Span,
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /// `(match scrutinee (pattern1 expr1) (pattern2 expr2) ...)`
    Match {
        span: Span,
        scrutinee: Box<Expr>,
        arms: Vec<(Literal, Expr)>,
    },
    /// `(foreach item collection body)`
    Foreach {
        span: Span,
        item: String,
        collection: Box<Expr>,
        body: Box<Expr>,
    },
    /// `(retry :times N :backoff <spec> <body>)`
    Retry {
        span: Span,
        times: u32,
        backoff: Option<String>,
        body: Box<Expr>,
    },
    /// `(catch :on <ErrClass> :fallback <expr> <body>)`
    Catch {
        span: Span,
        on: Option<String>,
        fallback: Box<Expr>,
        body: Box<Expr>,
    },
    /// `(llm arg1 arg2 ... :key value ...)` — a builtin form for the LLM tool.
    Llm {
        span: Span,
        positional: Vec<Expr>,
        args: KwArgs,
    },
    /// `(return expr)`
    Return { span: Span, value: Box<Expr> },
    /// A literal in expression position.
    Literal { span: Span, lit: Literal },
    /// A reference to a bound name (from `let` or a `define` param).
    Var { span: Span, name: String },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Tool { span, .. }
            | Expr::Pipe { span, .. }
            | Expr::Par { span, .. }
            | Expr::Let { span, .. }
            | Expr::If { span, .. }
            | Expr::Match { span, .. }
            | Expr::Foreach { span, .. }
            | Expr::Retry { span, .. }
            | Expr::Catch { span, .. }
            | Expr::Llm { span, .. }
            | Expr::Return { span, .. }
            | Expr::Literal { span, .. }
            | Expr::Var { span, .. } => *span,
        }
    }
}

/// A parsed .agnes file.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub toplevels: Vec<TopLevel>,
    /// The final expression at file end — the workflow entry point.
    /// None means the file only registers declares/defines.
    pub main: Option<Expr>,
}

impl fmt::Display for TypeExprAst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeExprAst::Named(n) => write!(f, "{n}"),
            TypeExprAst::App { head, args } => {
                write!(f, "({head}")?;
                for a in args {
                    write!(f, " {a}")?;
                }
                write!(f, ")")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_can_hold_a_pipe_of_two_tools() {
        // A minimal Program: (pipe (tool read-file :path "x") (tool summarize))
        let p = Program {
            toplevels: vec![],
            main: Some(Expr::Pipe {
                span: Span { start: 0, end: 0 },
                steps: vec![
                    Expr::Tool {
                        span: Span { start: 0, end: 0 },
                        name: "read-file".into(),
                        positional: vec![],
                        args: vec![(
                            "path".into(),
                            Expr::Literal {
                                span: Span { start: 0, end: 0 },
                                lit: Literal::String("x".into()),
                            },
                        )],
                    },
                    Expr::Tool {
                        span: Span { start: 0, end: 0 },
                        name: "summarize".into(),
                        positional: vec![],
                        args: vec![],
                    },
                ],
            }),
        };
        assert_eq!(p.toplevels.len(), 0);
        assert!(matches!(p.main, Some(Expr::Pipe { .. })));
    }
}

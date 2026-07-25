use agnes_ast::{Expr, Literal, Span};
use agnes_types::TypeExpr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

#[derive(Debug, Clone)]
pub enum NodeKind {
    Tool {
        name: String,
    },
    Pipe,
    Par,
    Let {
        name: String,
    },
    If,
    Match {
        arms: Vec<Literal>,
    },
    Foreach {
        item: String,
    },
    Retry {
        times: u32,
        backoff: Option<String>,
    },
    Catch {
        on: Option<String>,
        fallback: NodeId,
    },
    Return,
    /// `(finish X)` — wraps the child's runtime type in `Finish T` so the
    /// session loop's `classify_root` treats it as a terminating iteration.
    /// Single input: the child expression.
    Finish,
    /// `(observe X)` — wraps the child's runtime type in `Observation T`
    /// so the session loop feeds the value back to the planner.
    Observe,
    /// `(fmap expr)` — functor lift over an upstream Outcome.
    /// The child `Expr` is stored inline so the runtime can evaluate it
    /// via `eval_expr` with a dynamically-threaded upstream (the inner
    /// value extracted from the Outcome). Single input: the upstream
    /// Outcome node. Provides is `App { head: wrapper_head, args: [child_provides] }`.
    Fmap {
        child: Box<Expr>,
    },
    /// `(tool_observe name args...)` — tool + observe combinator.
    /// Inputs are kwargs (like a Tool node). Provides is
    /// `App { head: "Observation", args: [tool_provides] }`.
    ToolObserve {
        name: String,
    },
    Literal(Literal),
    Var(String),
    /// `(list e1 e2 ...)` — inputs are one `Input::FromNode` per element.
    /// Provides is `(List T)` where T comes from checker-determined types
    /// baked into `Node.provides`.
    List,
}

#[derive(Debug, Clone)]
pub enum Input {
    FromNode(NodeId),
    /// Reserved for future use (define inlining, literal argument optimization).
    /// Not currently constructed by the lowering — kwargs and pipe flow all
    /// resolve to Input::FromNode or Input::Kw. See docs/superpowers/plans/2026-07-18-agnes-dsl-mvp.md.
    Literal(Literal),
    /// Reserved for future use (define inlining, literal argument optimization).
    /// Not currently constructed by the lowering — kwargs and pipe flow all
    /// resolve to Input::FromNode or Input::Kw. See docs/superpowers/plans/2026-07-18-agnes-dsl-mvp.md.
    Var(String),
    /// Keyword-bound edge: same as FromNode but tagged with the parameter
    /// name so the runtime knows which slot to fill.
    Kw {
        key: String,
        source: Box<Input>,
    },
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub inputs: Vec<Input>,
    pub provides: TypeExpr,
}

#[derive(Debug, Clone)]
pub struct Dag {
    pub nodes: Vec<Node>,
    pub root: NodeId,
    /// Maps `NodeId` → source `Span` of the original `Expr` that produced
    /// this node. Used for post-execution branch-pruned DSL rendering.
    pub node_spans: Vec<Span>,
}

impl Dag {
    pub fn get(&self, id: NodeId) -> &Node {
        &self.nodes[id.0]
    }
}

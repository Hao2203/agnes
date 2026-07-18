use agnes_ast::Literal;
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
    /// Inputs are all `Input::Kw` entries with keys matching the llm builtin's
    /// parameter names (`prompt`, `input`). No positional inputs.
    Llm,
    Return,
    Literal(Literal),
    Var(String),
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
}

impl Dag {
    pub fn get(&self, id: NodeId) -> &Node {
        &self.nodes[id.0]
    }
}

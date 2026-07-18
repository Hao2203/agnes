use agnes_ast::Literal;
use agnes_types::TypeExpr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

#[derive(Debug, Clone)]
pub enum NodeKind {
    Tool { name: String },
    Pipe,
    Par,
    Let { name: String },
    If,
    Match { arms: Vec<Literal> },
    Foreach { item: String },
    Retry { times: u32, backoff: Option<String> },
    Catch { on: Option<String>, fallback: NodeId },
    Llm,
    Return,
    Literal(Literal),
    Var(String),
}

#[derive(Debug, Clone)]
pub enum Input {
    FromNode(NodeId),
    Literal(Literal),
    Var(String),
    /// Keyword-bound edge: same as FromNode but tagged with the parameter
    /// name so the runtime knows which slot to fill.
    Kw { key: String, source: Box<Input> },
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

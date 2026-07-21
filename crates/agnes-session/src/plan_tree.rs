use agnes_compiler::{Dag, Input, NodeKind};

#[derive(Debug, Clone)]
pub struct PlanTree {
    pub kind: String,
    pub label: String,
    pub provides: Option<String>,
    pub children: Vec<PlanTree>,
}

pub fn build_plan_tree(dag: &Dag) -> PlanTree {
    build(dag, dag.root)
}

fn build(dag: &Dag, id: agnes_compiler::NodeId) -> PlanTree {
    let node = dag.get(id);
    let (kind, label) = match &node.kind {
        NodeKind::Tool { name } => ("tool".into(), format!("tool {name}")),
        NodeKind::Llm => ("llm".into(), "llm".into()),
        NodeKind::Pipe => ("pipe".into(), "pipe".into()),
        NodeKind::Par => ("par".into(), "par".into()),
        NodeKind::Let { name } => ("let".into(), format!("let {name}")),
        NodeKind::If => ("if".into(), "if".into()),
        NodeKind::Match { .. } => ("match".into(), "match".into()),
        NodeKind::Foreach { item } => ("foreach".into(), format!("foreach {item}")),
        NodeKind::Retry { times, .. } => ("retry".into(), format!("retry {times}")),
        NodeKind::Catch { .. } => ("catch".into(), "catch".into()),
        NodeKind::Return => ("return".into(), "return".into()),
        NodeKind::Finish => ("finish".into(), "finish".into()),
        NodeKind::Observe => ("observe".into(), "observe".into()),
        NodeKind::Literal(lit) => ("lit".into(), format!("{lit:?}")),
        NodeKind::Var(n) => ("var".into(), n.clone()),
        NodeKind::List => ("list".into(), "list".into()),
    };
    let mut children = Vec::new();
    for inp in &node.inputs {
        if let Some(child_id) = child_id_of(inp) {
            children.push(build(dag, child_id));
        }
    }
    PlanTree {
        kind,
        label,
        provides: Some(node.provides.to_string()),
        children,
    }
}

fn child_id_of(inp: &Input) -> Option<agnes_compiler::NodeId> {
    match inp {
        Input::FromNode(id) => Some(*id),
        Input::Kw { source, .. } => child_id_of(source),
        Input::Literal(_) | Input::Var(_) => None,
    }
}

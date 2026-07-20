use crate::plan_tree::PlanTree;

#[derive(Debug, Clone)]
pub enum NodeKindTag {
    Tool { name: String },
    Llm,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    PlannerStart,
    PlannerRetry {
        attempt: u8,
        error: String,
    },
    DslProduced {
        source: String,
    },
    PlanReady {
        tree: PlanTree,
    },
    NodeStart {
        id: u32,
        kind: NodeKindTag,
        args: Vec<(String, String)>,
    },
    NodeEnd {
        id: u32,
        ok: bool,
        preview: String,
        elapsed_ms: u64,
    },
    TurnResult {
        value_preview: String,
        value_type: String,
    },
    TurnFailed {
        error: String,
    },
}

#[async_trait::async_trait]
pub trait EventSink: Send {
    async fn emit(&mut self, ev: SessionEvent);
}

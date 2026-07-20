use crate::events::{EventSink, NodeKindTag, SessionEvent};
use agnes_compiler::{NodeId, NodeKind};
use agnes_runtime::{RuntimeError, Tracer};
use agnes_types::Value;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

/// A tracer that forwards NodeStart/NodeEnd events over an in-memory
/// channel. The receiver side is drained by Session::run_turn and forwarded
/// to the user-supplied EventSink (which is `&mut dyn EventSink`, so it
/// cannot be shared across the sync callback boundary directly).
pub struct ChannelTracer {
    tx: Mutex<mpsc::UnboundedSender<SessionEvent>>,
}

impl ChannelTracer {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<SessionEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx: Mutex::new(tx) }, rx)
    }
}

impl Tracer for ChannelTracer {
    fn node_start(&self, id: NodeId, kind: &NodeKind, args_preview: &str) {
        let tag = match kind {
            NodeKind::Tool { name } => NodeKindTag::Tool { name: name.clone() },
            NodeKind::Llm => NodeKindTag::Llm,
            _ => return,
        };
        let args: Vec<(String, String)> = if args_preview.is_empty() {
            vec![]
        } else {
            vec![("preview".into(), args_preview.to_string())]
        };
        let _ = self.tx.lock().unwrap().send(SessionEvent::NodeStart {
            id: id.0 as u32,
            kind: tag,
            args,
        });
    }

    fn node_end(&self, id: NodeId, result: Result<&Value, &RuntimeError>, elapsed: Duration) {
        let (ok, preview) = match result {
            Ok(v) => {
                let p = if let Some(s) = v.data.as_str() {
                    let take: String = s.chars().take(60).collect();
                    format!("{}({}) {take:?}", v.declared_type, s.len())
                } else {
                    format!("{}", v.declared_type)
                };
                (true, p)
            }
            Err(e) => (false, e.to_string()),
        };
        let _ = self.tx.lock().unwrap().send(SessionEvent::NodeEnd {
            id: id.0 as u32,
            ok,
            preview,
            elapsed_ms: elapsed.as_millis() as u64,
        });
    }
}

pub async fn drain(rx: &mut mpsc::UnboundedReceiver<SessionEvent>, sink: &mut dyn EventSink) {
    while let Ok(ev) = rx.try_recv() {
        sink.emit(ev).await;
    }
}

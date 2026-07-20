use crate::error::LlmError;
use crate::provider::{CompletionRequest, Provider};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Deterministic in-memory provider for tests and demos.
/// Returns queued responses in FIFO order. Records every request it saw
/// so tests can assert on them.
#[derive(Debug, Clone)]
pub struct MockProvider {
    inner: Arc<Mutex<MockInner>>,
}

#[derive(Debug)]
struct MockInner {
    responses: VecDeque<String>,
    seen: Vec<CompletionRequest>,
}

impl MockProvider {
    pub fn new(responses: Vec<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockInner {
                responses: responses.into(),
                seen: Vec::new(),
            })),
        }
    }

    /// Snapshot of every request the mock has served so far, in order.
    pub fn seen(&self) -> Vec<CompletionRequest> {
        self.inner.lock().unwrap().seen.clone()
    }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<String, LlmError> {
        let mut g = self.inner.lock().unwrap();
        g.seen.push(req);
        Ok(g.responses
            .pop_front()
            .expect("MockProvider: response queue exhausted; queue more responses"))
    }
}

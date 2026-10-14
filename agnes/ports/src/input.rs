//! Out-of-band user input while a run is executing.

use agnes_core::content::Content;
use async_trait::async_trait;

/// How the driver learns what the user did while it was working: messages
/// typed ahead (injected at the next safe boundary) and stop requests (raced
/// against every await point).
#[async_trait]
pub trait Input: Send + Sync {
    /// Takes the messages typed since the last call, in order. Never blocks;
    /// returns an empty vector when there are none — leftovers stay queued
    /// for the next run.
    fn drain(&self) -> Vec<Content>;

    /// Resolves once the user has asked to stop — immediately if the request
    /// already happened (level-triggered).
    async fn interrupted(&self);
}

/// For hosts with no interactive user (headless runs): never yields
/// messages, never interrupts.
pub struct NoInput;

#[async_trait]
impl Input for NoInput {
    fn drain(&self) -> Vec<Content> {
        Vec::new()
    }

    async fn interrupted(&self) {
        std::future::pending::<()>().await
    }
}

//! Regression test for the `shell-run` sink-mutex deadlock.
//!
//! `shell-run` re-enters the turn's event sink through
//! `PathResolver::emit_shell_confirm` to request confirmation. If the
//! turn task holds the sink `Mutex` guard across `execute_with` (as it
//! once did), the tool's confirm request deadlocks waiting for that
//! same mutex, and the turn hangs forever with no output. This test
//! runs a `shell-run` turn under a sink that auto-approves and asserts
//! the turn completes within a deadline.

use agnes_llm::{MockProvider, Provider};
use agnes_session::{EventSink, Session, SessionEvent, TurnInput};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// A sink that silently auto-approves every `ShellConfirm` so `shell-run`
/// can proceed without a human at stdin.
struct AutoApproveSink;

#[async_trait]
impl EventSink for AutoApproveSink {
    async fn emit(&mut self, ev: SessionEvent) {
        if let SessionEvent::ShellConfirm { responder, .. } = ev {
            if let Some(tx) = Arc::into_inner(responder) {
                let _ = tx.send(true);
            }
        }
    }
}

#[tokio::test]
async fn shell_run_completes_during_turn() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));
    let mut session = Session::new(provider).unwrap().with_allow_shell(true);

    let sink = Arc::new(Mutex::new(AutoApproveSink));
    let dsl = "(tool shell-run \"echo hi\")";

    // Before the fix, this deadlocked: the turn held the sink mutex for
    // the whole turn while `shell-run` awaited the same mutex to emit
    // `ShellConfirm`. The 10s deadline turns that hang into a failure.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        session.run_turn(TurnInput::RawDsl(dsl.into()), sink),
    )
    .await;

    let value = result.expect("shell-run turn deadlocked (sink mutex not released)");
    let value = value.expect("turn should succeed");
    assert!(
        value.declared_type.to_string().contains("CommandResult"),
        "expected CommandResult, got {}",
        value.declared_type
    );
}

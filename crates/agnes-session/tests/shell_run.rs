//! Regression tests for `shell-run`.
//!
//! 1. `shell-run` re-enters the turn's event sink through the `Sink`
//!    trait to request confirmation. Two deadlock bugs once lived here:
//!    the turn holding the sink `Mutex` guard across `execute_with`, and
//!    the per-iteration drain running on the turn task so a tool holding
//!    the lock across a long await starved it. We run a `shell-run` turn
//!    under a sink that auto-approves and assert it completes within a
//!    deadline.
//!
//! 2. `shell-run` streams each output line live as it is produced
//!    (rather than buffering until exit), so a long build does not look
//!    hung. We assert each line arrives as its own `ShellOutput` event
//!    in order, and that the final `CommandResult` captures the full
//!    output.

use agnes_llm::{MockProvider, Provider};
use agnes_session::{EventSink, Session, SessionEvent, TurnInput};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A sink that auto-approves every `ShellConfirm` (so `shell-run` can
/// proceed without a human at stdin) and records every `ShellOutput`
/// line (with its stream origin) for assertions. The recorded lines live
/// behind a shared `Arc<Mutex>` so the test can inspect them after the
/// sink has been moved (boxed) into `run_turn`.
struct AutoApproveSink {
    lines: Arc<Mutex<Vec<(String, bool)>>>,
}

impl AutoApproveSink {
    /// Returns the sink and a shared handle to its recorded lines.
    fn new() -> (Self, Arc<Mutex<Vec<(String, bool)>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        (Self { lines: lines.clone() }, lines)
    }
}

#[async_trait]
impl EventSink for AutoApproveSink {
    async fn emit(&mut self, ev: SessionEvent) {
        match ev {
            SessionEvent::ShellConfirm { responder, .. } => {
                if let Some(tx) = Arc::into_inner(responder) {
                    let _ = tx.send(true);
                }
            }
            SessionEvent::ShellOutput { line, is_stderr } => {
                self.lines.lock().unwrap().push((line, is_stderr))
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn shell_run_completes_and_streams_during_turn() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));
    let mut session = Session::new(provider).unwrap().with_allow_shell(true);

    let (sink, lines) = AutoApproveSink::new();
    let dsl = "(tool shell-run \"echo hi\")";

    // The 10s deadline turns any deadlock hang into a failure.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        session.run_turn(TurnInput::RawDsl(dsl.into()), Box::new(sink)),
    )
    .await;

    let value = result.expect("shell-run turn deadlocked (sink mutex not released)");
    let value = value.expect("turn should succeed");
    assert!(
        value.declared_type.to_string().contains("CommandResult"),
        "expected CommandResult, got {}",
        value.declared_type
    );

    // The line was both streamed live and captured in the final result.
    let stdout = value.data["stdout"]
        .as_str()
        .expect("CommandResult should have a stdout field");
    assert!(
        stdout.contains("hi"),
        "stdout should contain 'hi', got {stdout:?}"
    );
    let lines = lines.lock().unwrap().clone();
    assert!(
        lines.iter().any(|(l, _)| l.contains("hi")),
        "expected a streamed ShellOutput line containing 'hi', got {lines:?}"
    );
}

#[tokio::test]
async fn shell_run_streams_lines_individually() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));
    let mut session = Session::new(provider).unwrap().with_allow_shell(true);

    let (sink, lines) = AutoApproveSink::new();
    let dsl = "(tool shell-run \"echo a; echo b; echo c\")";

    let value = tokio::time::timeout(
        Duration::from_secs(10),
        session.run_turn(TurnInput::RawDsl(dsl.into()), Box::new(sink)),
    )
    .await
    .expect("shell-run turn deadlocked (sink mutex not released)")
    .expect("turn should succeed");

    // Each line arrives as its own ShellOutput event, in order, on stdout.
    let rec = lines.lock().unwrap().clone();
    assert_eq!(
        rec,
        vec![
            ("a".to_string(), false),
            ("b".to_string(), false),
            ("c".to_string(), false),
        ],
        "each line should be streamed individually on stdout, got {rec:?}"
    );

    // And the full output is still captured in the CommandResult.
    let stdout = value.data["stdout"]
        .as_str()
        .expect("CommandResult should have a stdout field");
    assert_eq!(stdout, "a\nb\nc\n");
}

#[tokio::test]
async fn shell_run_streams_stderr_separately() {
    // cargo writes its `Compiling ...` progress to stderr; verify that
    // stream is plumbed through with is_stderr = true.
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));
    let mut session = Session::new(provider).unwrap().with_allow_shell(true);

    let (sink, lines) = AutoApproveSink::new();
    let dsl = "(tool shell-run \"echo err 1>&2\")";

    tokio::time::timeout(
        Duration::from_secs(10),
        session.run_turn(TurnInput::RawDsl(dsl.into()), Box::new(sink)),
    )
    .await
    .expect("shell-run turn deadlocked (sink mutex not released)")
    .expect("turn should succeed");

    let rec = lines.lock().unwrap().clone();
    assert_eq!(
        rec,
        vec![("err".to_string(), true)],
        "stderr line should be streamed with is_stderr=true, got {rec:?}"
    );
}

#[tokio::test]
async fn shell_run_survives_slow_confirm_that_holds_the_sink_lock() {
    // The real StderrEventSink holds the sink mutex across the blocking
    // stdin read in ShellConfirm. A sink whose confirm awaits before
    // approving reproduces that (the Session's `shell_confirm` does
    // `sink.lock().await.emit(ShellConfirm).await`, so the guard spans the
    // await). With the drain task on the turn task (the old design) this
    // deadlocked: drain blocked on the lock the tool held, the turn task
    // stopped polling the tool, and the 200ms sleep never resolved. With
    // drain on its own task there is no same-task contender, so it
    // completes. This test pins the structural fix.
    struct SlowConfirmSink;
    #[async_trait]
    impl EventSink for SlowConfirmSink {
        async fn emit(&mut self, ev: SessionEvent) {
            match ev {
                SessionEvent::ShellConfirm { responder, .. } => {
                    // Hold the sink lock across an await, like a blocking
                    // stdin read would.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if let Some(tx) = Arc::into_inner(responder) {
                        let _ = tx.send(true);
                    }
                }
                _ => {}
            }
        }
    }

    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![]));
    let mut session = Session::new(provider).unwrap().with_allow_shell(true);
    let dsl = "(tool shell-run \"echo ok\")";

    let value = tokio::time::timeout(
        Duration::from_secs(5),
        session.run_turn(TurnInput::RawDsl(dsl.into()), Box::new(SlowConfirmSink)),
    )
    .await
    .expect("shell-run deadlocked when the sink held its lock across confirm")
    .expect("turn should succeed");

    let stdout = value.data["stdout"]
        .as_str()
        .expect("CommandResult should have a stdout field");
    assert!(stdout.contains("ok"), "stdout should contain 'ok', got {stdout:?}");
}

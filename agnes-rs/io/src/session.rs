//! Async seam for appending a run's event log to durable storage.
//!
//! The seam is deliberately tiny: the driver owns the ordering and the
//! session format owns the envelope. Two promises, and nothing else:
//!
//! - [`SessionLog::append`] / [`SessionLog::append_host`] are **best effort**
//!   — returning from them does not promise crash durability.
//! - [`SessionLog::flush`] is the **durability boundary**: after it returns,
//!   everything appended so far survives a crash. The driver flushes before
//!   every model request and when the run ends.
//!
//! The log carries two vocabularies: core events ([`Event`]) are the run's
//! truth, and [`HostEvent`]s are the audit trail core must not know about
//! (approval decisions, the cause of an aborted run). They share one order.

use agnes_core::event::Event;
use agnes_core::message::ToolCallId;
use agnes_core::tool::ToolName;
use async_trait::async_trait;
use jiff::Timestamp;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    #[error("session i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session record could not be encoded")]
    Encode(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("session writer is poisoned after a failed write")]
    Poisoned,
    #[error("session sequence number exhausted")]
    SequenceExhausted,
}

/// How an approval ask settled. Fail-closed: only
/// [`ApprovalOutcome::AllowedOnce`] lets the call run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// Explicitly approved once.
    AllowedOnce,
    /// Explicitly refused.
    Rejected,
    /// The wait was cancelled (the run was interrupted).
    Cancelled,
    /// No answerer: not wired, channel closed, reply dropped, or timed out.
    Unavailable,
}

impl ApprovalOutcome {
    /// The one value that allows the call.
    pub fn allows(self) -> bool {
        matches!(self, Self::AllowedOnce)
    }
}

/// Audit records that belong to the host, not to the core vocabulary.
///
/// They never extend [`Event`]: plugins and approvals are host semantics, and
/// core stays closed. The session format interleaves both by one sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// An approval question was handed to the answerer.
    ApprovalRequested { call_id: ToolCallId, tool: ToolName },
    /// The question settled (including cancellation and unavailability).
    ApprovalResolved {
        call_id: ToolCallId,
        outcome: ApprovalOutcome,
    },
    /// The run stopped with an error (model, transport or session write).
    /// Core does not model I/O failures, so the cause lives here.
    Failed { cause: String },
}

/// Append-only sink for one session's event log.
#[async_trait]
pub trait SessionLog: Send {
    /// Appends one core event, stamped by the host's clock.
    async fn append(&mut self, at: Timestamp, event: &Event) -> Result<(), SessionError>;

    /// Appends one host-semantic audit record, sharing the same order.
    async fn append_host(&mut self, at: Timestamp, event: &HostEvent) -> Result<(), SessionError>;

    /// Makes everything appended so far durable.
    async fn flush(&mut self) -> Result<(), SessionError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::event::Event;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Memory {
        events: Arc<Mutex<Vec<Event>>>,
        notes: Arc<Mutex<Vec<HostEvent>>>,
        flushes: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl SessionLog for Memory {
        async fn append(&mut self, _at: Timestamp, event: &Event) -> Result<(), SessionError> {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }

        async fn append_host(
            &mut self,
            _at: Timestamp,
            event: &HostEvent,
        ) -> Result<(), SessionError> {
            self.notes.lock().unwrap().push(event.clone());
            Ok(())
        }

        async fn flush(&mut self) -> Result<(), SessionError> {
            *self.flushes.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_session_log_receives_events_in_order_and_counts_flushes() {
        let memory = Memory::default();
        let mut log: Box<dyn SessionLog> = Box::new(memory.clone());

        log.append(Timestamp::now(), &Event::TurnStart)
            .await
            .unwrap();
        log.append_host(
            Timestamp::now(),
            &HostEvent::ApprovalRequested {
                call_id: ToolCallId::new("c1"),
                tool: ToolName::from_str("run_shell").unwrap(),
            },
        )
        .await
        .unwrap();
        log.flush().await.unwrap();
        log.append(
            Timestamp::now(),
            &Event::TurnEnd {
                outcome: agnes_core::turn::Outcome::Interrupted,
            },
        )
        .await
        .unwrap();

        assert_eq!(memory.events.lock().unwrap().len(), 2);
        assert_eq!(memory.notes.lock().unwrap().len(), 1);
        assert_eq!(*memory.flushes.lock().unwrap(), 1);
        assert!(!ApprovalOutcome::Rejected.allows());
        assert!(ApprovalOutcome::AllowedOnce.allows());
    }
}

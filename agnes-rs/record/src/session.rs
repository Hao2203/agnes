//! Async seam for appending a run's record to durable storage.
//!
//! The seam is deliberately tiny: the writer owns the ordering and the
//! session format owns the envelope. Two promises, and nothing else:
//!
//! - [`SessionLog::append`] / [`SessionLog::append_host`] are **best effort**
//!   — returning from them does not promise crash durability.
//! - [`SessionLog::flush`] is the **durability boundary**: after it returns,
//!   everything appended so far survives a crash. The writer flushes before
//!   every model request and when the run ends.
//!
//! The log carries two vocabularies: record events ([`Event`]) are the run's
//! truth, and [`HostEvent`]s are the audit trail core must not know about
//! (approval decisions, the cause of an aborted run). They share one order.

use std::path::PathBuf;
use std::sync::Arc;

use crate::event::{Event, HostEvent};
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

/// Append-only sink for one session's record.
#[async_trait]
pub trait SessionLog: Send {
    /// Appends one record event, stamped by the host's clock.
    async fn append(&mut self, at: Timestamp, event: &Event) -> Result<(), SessionError>;

    /// Appends one host-semantic audit record, sharing the same order.
    async fn append_host(&mut self, at: Timestamp, event: &HostEvent) -> Result<(), SessionError>;

    /// Makes everything appended so far durable.
    async fn flush(&mut self) -> Result<(), SessionError>;
}

/// A durable store for one session's record, provided by a persistence
/// plugin and driven by the session plugin's writer task.
///
/// The writer owns the ordering and calls the store sequentially. `flush` is
/// the durability boundary; `rotate` flushes the current storage, claims the
/// next one, and opens it.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Appends one record event, stamped by the host's clock.
    async fn append(&self, at: Timestamp, event: &Event) -> Result<(), SessionError>;

    /// Appends one host-semantic audit record, sharing the same order.
    async fn append_host(&self, at: Timestamp, event: &HostEvent) -> Result<(), SessionError>;

    /// Makes everything appended so far durable.
    async fn flush(&self) -> Result<(), SessionError>;

    /// Flushes the current storage, claims the next one, and opens it.
    /// Returns the new location.
    async fn rotate(&self) -> Result<PathBuf, SessionError>;
}

/// The installed session store as a shareable handle.
///
/// A persistence plugin mounts one implementation into the world; the session
/// plugin's writer reads it. The type is plain data — it becomes a resource
/// only where an ECS installs it.
pub struct SessionStoreResource(Arc<dyn SessionStore>);

impl SessionStoreResource {
    /// Wraps a store implementation.
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self(store)
    }

    /// The installed store.
    pub fn store(&self) -> &Arc<dyn SessionStore> {
        &self.0
    }
}

/// The durability progress between the recording side and the writer.
///
/// The recording side forwards session-relevant items under a monotonically
/// increasing sequence number; the writer acknowledges the last sequence it
/// made durable. A model request may only leave after everything sent so
/// far is acknowledged — the same commit boundary the driver enforced.
#[derive(Debug, Clone, Default)]
pub struct Durability {
    /// The last sequence number forwarded to the writer.
    pub sent_seq: u64,
    /// The last sequence number the writer made durable.
    pub flushed_seq: u64,
    /// Why the session became unwritable, set once and never cleared.
    pub failed: Option<String>,
}

impl Durability {
    /// Whether everything sent so far is durable and the session is healthy.
    pub fn is_durable(&self) -> bool {
        self.failed.is_none() && self.flushed_seq >= self.sent_seq
    }
}

/// The host asked to rotate the session (`/new`): drop the in-memory
/// context and, where a session file exists, continue in a fresh one.
///
/// The runtime resets its [`Log`](crate::Log); a persistence plugin clears
/// its durability baseline and swaps files. Hosts inject this through
/// `RuntimeSender`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RotateSession;

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::message::ToolCallId;
    use agnes_core::tool::ToolName;
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
        assert!(!crate::ApprovalOutcome::Rejected.allows());
        assert!(crate::ApprovalOutcome::AllowedOnce.allows());
    }
}

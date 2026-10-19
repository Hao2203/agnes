//! Async seam for appending a run's event log to durable storage.
//!
//! The seam is deliberately tiny: the driver owns the ordering and the
//! session format owns the envelope. Two promises, and nothing else:
//!
//! - [`SessionLog::append`] is **best effort** — returning from it does not
//!   promise crash durability.
//! - [`SessionLog::flush`] is the **durability boundary**: after it returns,
//!   everything appended so far survives a crash. The driver flushes before
//!   every model request and when the run ends.

use agnes_core::event::Event;
use async_trait::async_trait;
use jiff::Timestamp;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed session data: {0}")]
    Format(String),
}

/// Append-only sink for one session's event log.
#[async_trait]
pub trait SessionLog: Send {
    /// Appends one event, stamped by the host's clock.
    async fn append(&mut self, at: Timestamp, event: &Event) -> Result<(), SessionError>;

    /// Makes everything appended so far durable.
    async fn flush(&mut self) -> Result<(), SessionError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::event::Event;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Memory {
        events: Arc<Mutex<Vec<Event>>>,
        flushes: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl SessionLog for Memory {
        async fn append(&mut self, _at: Timestamp, event: &Event) -> Result<(), SessionError> {
            self.events.lock().unwrap().push(event.clone());
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
        log.flush().await.unwrap();
        log.append(
            Timestamp::now(),
            &Event::TurnEnd {
                outcome: agnes_core::turn::Outcome::Interrupted,
            },
        )
        .await
        .unwrap();

        let events = memory.events.lock().unwrap().clone();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], Event::TurnStart));
        assert_eq!(*memory.flushes.lock().unwrap(), 1);
    }
}

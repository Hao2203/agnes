//! Session state as an ECS plugin.
//!
//! [`SessionPlugin`] owns the session's state: the in-memory [`Log`], the
//! [`Durability`] progress, the sequence numbers, and the rotation policy. It
//! never touches storage itself — persistence is a separate plugin that mounts
//! a [`SessionStoreResource`]; without one the session runs in memory-only
//! mode (the `Log` still records, nothing is written, and the durability gate
//! is always open).
//!
//! The `session_pump` system forwards the run's [`RunEvent`] stream to an
//! ordered writer task over a bounded channel — the system never awaits — and
//! the writer calls the installed store, flushes per batch, and acknowledges
//! progress through [`SessionFlushed`] messages. The [`Durability`] resource
//! tells the agent runtime when everything sent so far is durable, restoring
//! the driver's "flush before the request leaves" commit boundary.
//!
//! Failures are fail-closed: once the store reports a write failure the
//! [`Durability::failed`] reason is set and never cleared; the run closes
//! with an error instead of continuing without an audit trail.
//!
//! # Rotation
//!
//! The host sends a [`RotateSession`] message (`/new`): the in-memory [`Log`]
//! resets and the durability baseline returns to zero; a persistence plugin's
//! store flushes the current storage and opens the next one.
//!
//! # Ordering
//!
//! Plugins apply immediately in registration order. Add this plugin
//! **before** `agnes_agent::runtime::AgentPlugin` and the renderer plugin:
//! its `session_ack` system must run first in a tick so a flush
//! acknowledgement is visible to the durability gate and the final report
//! within the same tick.

use agnes_ecs::{App, Plugin, Update};
use agnes_record::{Durability, Event, Log};
use jiff::Timestamp;

mod systems;

#[cfg(test)]
mod tests;

pub use agnes_record::RotateSession;
pub use systems::{SessionFlushed, SessionRotated, SessionWriteFailed};

/// Capacity of the channel between the pump and the writer. The writer
/// drains per schedule tick, so a healthy session never comes close; hitting
/// the cap fails the session (fail-closed) rather than blocking a system.
const CHANNEL_CAPACITY: usize = 256;

/// One command for the writer task.
pub(crate) enum SessionCommand {
    Append {
        seq: u64,
        at: Timestamp,
        event: Event,
    },
    AppendHost {
        seq: u64,
        at: Timestamp,
        event: agnes_record::HostEvent,
    },
    Rotate {
        seq: u64,
    },
}

/// The pump's handle onto the writer task; the writer starts lazily on the
/// first tick with a store installed.
#[derive(Default)]
pub struct SessionPump {
    sender: Option<async_channel::Sender<SessionCommand>>,
}

/// The session's state: log, durability, and rotation policy.
///
/// Without a [`SessionStoreResource`] the session is memory-only; with one,
/// the writer persists through it.
pub struct SessionPlugin {
    events: Vec<Event>,
}

impl Default for SessionPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionPlugin {
    /// Creates a memory-only session: state without persistence.
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Resumes from restored history (seeds the [`Log`] and the durability
    /// baseline).
    ///
    /// # Errors
    ///
    /// Returns [`SessionInitError::NoHeader`] when the history has records
    /// but no [`Event::Header`] — a session that cannot be continued.
    pub fn resumed(events: Vec<Event>) -> Result<Self, SessionInitError> {
        if events.is_empty() || events.iter().any(|event| matches!(event, Event::Header(_))) {
            return Ok(Self { events });
        }
        Err(SessionInitError::NoHeader)
    }
}

/// A session could not be initialized.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionInitError {
    #[error("session history has records but no header")]
    NoHeader,
}

impl Plugin for SessionPlugin {
    fn name(&self) -> &'static str {
        "agnes-session"
    }

    fn build(&self, app: &mut App) {
        // 恢复的历史事件已经在存储里 —— 耐久序号从它们之后起算。
        let restored =
            u64::try_from(self.events.len()).expect("the restored event count fits into u64");
        app.insert_resource(Log::restore(self.events.clone()))
            .insert_resource(SessionPump::default())
            .insert_resource(Durability {
                sent_seq: restored,
                flushed_seq: restored,
                failed: None,
            })
            .add_systems(Update, systems::session_pump)
            .add_systems(Update, systems::session_ack)
            .add_systems(Update, systems::session_rotate);
    }
}

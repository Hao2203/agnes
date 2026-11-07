//! Session systems as an ECS plugin.
//!
//! [`SessionPlugin`] registers the session's systems: the [`Durability`]
//! progress, the writer pump, and the rotation acknowledgement. The in-memory
//! [`Log`] is part of the agent's core state (`AgentCore`), not of this
//! plugin: the host passes the log returned by [`session_state`] into the core
//! state before adding this plugin. The plugin never touches storage itself —
//! persistence is a
//! [`SessionStoreResource`](agnes_record::SessionStoreResource) component the
//! host attaches on the same entity; without one the session runs in
//! memory-only mode (the `Log` still records, nothing is written, and the
//! durability gate is always open).
//!
//! The `session_pump` system forwards the run's
//! [`RunEvent`](agnes_record::RunEvent) stream to an ordered writer task over
//! a bounded channel — the system never awaits — and the writer calls the
//! installed store, flushes per batch, and acknowledges progress through
//! [`SessionFlushed`] messages addressed to the agent.
//! The [`Durability`] component tells the agent runtime when everything sent
//! so far is durable, restoring the driver's "flush before the request
//! leaves" commit boundary.
//!
//! Failures are fail-closed: once the store reports a write failure the
//! [`Durability::failed`] reason is set and never cleared; the run closes
//! with an error instead of continuing without an audit trail.
//!
//! # Rotation
//!
//! The host sends a [`RotateSession`] message (`/new`): the durability baseline
//! returns to zero and a persistence plugin's store flushes the current
//! storage and opens the next one. The in-memory [`Log`] belongs to the
//! agent's core state, so the agent runtime's `session_rotate_log` clears it;
//! this plugin's `session_rotate_durability` resets the baseline.
//!
//! # Ordering
//!
//! Plugins apply immediately in registration order. Add this plugin
//! **before** `agnes_agent::runtime::AgentPlugin` and the renderer plugin:
//! its `session_ack` system must run first in a tick so a flush
//! acknowledgement is visible to the durability gate and the final report
//! within the same tick. Both plugins assert this dependency at build time.

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

agnes_ecs::component!(SessionPump);

/// The session's state for one agent: log, pump, and durability.
///
/// The host spawns the agent, passes `log` into the agent's core state
/// (`AgentCore::new`), and attaches `pump` + `durability` before adding
/// [`SessionPlugin`]. Without a
/// [`SessionStoreResource`](agnes_record::SessionStoreResource) component on
/// the same entity the session is memory-only; with one, the writer persists
/// through it.
pub struct SessionState {
    /// In-memory session log; owned by the agent's `AgentCore`.
    pub log: Log,
    /// Handle onto the persistence writer task.
    pub pump: SessionPump,
    /// Durability progress and failure state.
    pub durability: Durability,
}

/// Builds the state for one agent session.
///
/// # Errors
///
/// Returns [`SessionInitError::NoHeader`] when the history has records but no
/// [`Event::Header`] — a session that cannot be continued.
///
/// # Panics
///
/// The restored event count must fit into `u64` — unreachable on every
/// supported target, where `usize` is at most 64 bits.
pub fn session_state(events: &[Event]) -> Result<SessionState, SessionInitError> {
    if !events.is_empty() && !events.iter().any(|event| matches!(event, Event::Header(_))) {
        return Err(SessionInitError::NoHeader);
    }
    // 恢复的历史事件已经在存储里 —— 耐久序号从它们之后起算。
    let restored = u64::try_from(events.len()).expect("the restored event count fits into u64");
    Ok(SessionState {
        log: Log::restore(events.to_vec()),
        pump: SessionPump::default(),
        durability: Durability {
            sent_seq: restored,
            flushed_seq: restored,
            failed: None,
        },
    })
}

/// A session could not be initialized.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionInitError {
    #[error("session history has records but no header")]
    NoHeader,
}

/// Registers the session systems; state comes from [`session_state`].
#[derive(Default)]
pub struct SessionPlugin;

impl Plugin for SessionPlugin {
    fn name(&self) -> &'static str {
        "agnes-session"
    }

    fn build(&self, app: &mut App) {
        app.add_systems(Update, systems::session_pump)
            .add_systems(Update, systems::session_ack)
            .add_systems(Update, systems::session_rotate_durability);
    }
}

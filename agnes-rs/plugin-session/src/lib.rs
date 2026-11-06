//! Session systems as an ECS plugin.
//!
//! [`SessionPlugin`] registers the session's systems: the in-memory [`Log`],
//! the [`Durability`] progress, and the rotation policy all hang as components
//! on the session's root entity — one root per running agent — never
//! in global resources. The host spawns the root and attaches the state with
//! [`session_state`] before adding this plugin; the plugin never touches
//! storage itself — persistence is a [`SessionStoreResource`] component the
//! host attaches on the same root; without one the session runs in memory-only
//! mode (the `Log` still records, nothing is written, and the durability gate
//! is always open).
//!
//! The `session_pump` system forwards the run's [`RunEvent`] stream to an
//! ordered writer task over a bounded channel — the system never awaits — and
//! the writer calls the installed store, flushes per batch, and acknowledges
//! progress through [`SessionFlushed`] messages addressed to the agent root.
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

/// The session's state for one agent root: log, pump, and durability.
///
/// The host spawns the root and attaches this bundle before adding
/// [`SessionPlugin`]. Without a [`SessionStoreResource`] component on the same
/// root the session is memory-only; with one, the writer persists through it.
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
pub fn session_state(events: &[Event]) -> Result<(Log, SessionPump, Durability), SessionInitError> {
    if !events.is_empty() && !events.iter().any(|event| matches!(event, Event::Header(_))) {
        return Err(SessionInitError::NoHeader);
    }
    // 恢复的历史事件已经在存储里 —— 耐久序号从它们之后起算。
    let restored = u64::try_from(events.len()).expect("the restored event count fits into u64");
    Ok((
        Log::restore(events.to_vec()),
        SessionPump::default(),
        Durability {
            sent_seq: restored,
            flushed_seq: restored,
            failed: None,
        },
    ))
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
            .add_systems(Update, systems::session_rotate);
    }
}

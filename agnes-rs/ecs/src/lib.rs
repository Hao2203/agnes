//! A small, single-threaded ECS runtime with Bevy-style typed systems.
//!
//! The crate is intentionally independent from the rest of the workspace: it
//! knows nothing about models, tools, or the agent loop. A reference agent
//! layer lives in [`demo`] and is built entirely on the public ECS API.
//!
//! # Typed systems
//!
//! Systems are ordinary synchronous Rust functions whose parameters declare
//! what they need:
//!
//! ```ignore
//! fn process_turn(
//!     mut turns: Query<&mut Turn>,
//!     config: Res<ModelConfig>,
//!     mut requests: MessageWriter<ModelRequest>,
//! ) {
//!     // use the parameters directly
//! }
//! ```
//!
//! The runtime initializes per-system state, extracts parameters from the
//! [`World`], checks access conflicts, and applies deferred [`Commands`].
//!
//! # Safety and borrows
//!
//! Everything is built on safe `RefCell` borrowing. Query items are
//! [`Ref`](std::cell::Ref) and [`RefMut`](std::cell::RefMut) rather than raw
//! references, but deref coercion keeps usage close to Bevy:
//!
//! ```ignore
//! for mut agent in query.iter_mut() {
//!     agent.active = true;
//! }
//! ```
//!
//! Holding two mutable borrows of the same component on the same entity will
//! panic instead of causing undefined behavior. Conflicting parameters inside
//! one system are rejected earlier, when the system is added to a [`Schedule`].
//!
//! # Messages and commands
//!
//! [`MessageWriter`] writes to a pending buffer that is published when the
//! schedule finishes, so readers observe those messages on the next run.
//! [`Commands`] queue structural changes and apply them after the system
//! returns, once all component borrows have been released.
//!
//! # Async tasks without a runtime
//!
//! The core never awaits inside the ECS: systems are synchronous, and async
//! work is submitted through [`AsyncExecutor`], which delegates to a
//! [`TaskSpawner`] installed with [`App::set_spawner`]. Ready-made spawners
//! live in [`adapters`] behind the `tokio` and `async-executor` features; any
//! other executor can be used by implementing [`TaskSpawner`] directly. The
//! [`Runner`] itself only needs a channel and a cancellation token, so it runs
//! under any executor.

mod access;
mod app;
mod commands;
mod component;
mod entity;
mod message;
mod plugin;
mod query;
mod resource;
mod runner;
mod schedule;
mod system;
mod system_param;
mod world;

pub mod adapters;
pub mod demo;

#[cfg(test)]
mod tests;

pub use access::{Access, AccessKind, AccessMode, AccessSet};
pub use app::{App, ScheduleLabel, Update};
pub use commands::{CommandQueue, Commands, EntityCommands};
pub use component::Component;
pub use entity::Entity;
pub use message::{Message, MessageReader, MessageWriter, ReaderId};
pub use plugin::Plugin;
pub use query::{Query, QueryData, QueryFilter, QueryIter, With, Without};
pub use resource::{Res, ResMut, Resource};
pub use runner::{
    AsyncExecutor, CancelToken, Runner, RunnerError, RuntimeSendError, RuntimeSender, SpawnError,
    TaskSpawner,
};
pub use schedule::{Schedule, ScheduleError};
pub use system::{FunctionSystem, IntoSystem, System, SystemError, SystemParamFunction};
pub use system_param::{ParamError, SystemParam};
pub use world::World;

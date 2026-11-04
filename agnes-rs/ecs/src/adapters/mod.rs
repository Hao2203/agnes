//! Optional glue between the runtime-agnostic core and concrete async runtimes.
//!
//! Each adapter is gated behind a feature of the same name and provides a
//! ready-made [`TaskSpawner`](crate::TaskSpawner) implementation:
//!
//! - `tokio`: a spawner for the Tokio runtime that is active when a task is
//!   spawned, plus a helper that runs an [`App`](crate::App) on a new
//!   current-thread runtime with I/O and time drivers enabled.
//! - `async-executor`: a spawner for a shared `async_executor::Executor`,
//!   which the host drives itself.
//!
//! Hosts using a different runtime can implement
//! [`TaskSpawner`](crate::TaskSpawner) directly instead.

#[cfg(feature = "async-executor")]
pub mod async_executor;
#[cfg(feature = "tokio")]
pub mod tokio;

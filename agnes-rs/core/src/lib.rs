//! Core of the agnes agent: domain types and the agent state machine.
//!
//! This crate is pure computation — synchronous, no I/O, no async. The
//! agent is modelled as a type-state machine ([`turn`]): every transition
//! consumes the current state and returns the next state together with an
//! *effect* (a model request or a batch of tool calls). The async I/O seams
//! that interpret those effects live in `agnes-io`; hosts wire them to
//! whatever runtime they like.

pub mod config;
pub mod content;
pub mod context;
pub mod llm;
pub mod message;
pub mod task;
pub mod tool;

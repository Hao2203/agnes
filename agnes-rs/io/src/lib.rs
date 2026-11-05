//! Async I/O seams for the agnes agent (hexagonal architecture).
//!
//! These are the seams hosts implement to give the agent its I/O: tools
//! ([`tool::Tool`]) and raw HTTP ([`http::HttpTransport`]), plus the
//! installable handle ([`http::HttpTransportResource`]) that plugins mount
//! into an ECS world. Model calls travel as messages instead
//! ([`llm::LlmCall`] / [`llm::LlmReply`]), with the codec in
//! `agnes-llm-openai` and the round trip in `agnes-plugin-http`. Seams are
//! async and `Send`; most are also `Sync`. Swapping runtimes means swapping
//! the implementations (see `agnes-transport-reqwest`), never `agnes-core`.
//!
//! The recording vocabulary and the durable session seam live in
//! `agnes-record`. User input is not a seam here: the ECS-based runtime
//! receives host-injected messages instead.

pub mod http;
pub mod llm;
pub mod tool;

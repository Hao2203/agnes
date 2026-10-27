//! Async I/O seams for the agnes agent (hexagonal architecture).
//!
//! These are the seams hosts implement to give the agent its I/O: model
//! access ([`llm::Llm`]), tools ([`tool::Tool`]), raw HTTP
//! ([`http::HttpTransport`]), and the user's out-of-band actions while a run
//! executes ([`input::Input`]). Seams are async and `Send`; most are also
//! `Sync`. Swapping runtimes means swapping the implementations (see
//! `agnes-transport-reqwest`), never `agnes-core`.
//!
//! The durable session seam lives in `agnes-agent`: its record vocabulary is
//! owned by the host, so the port moved with it.

pub mod http;
pub mod input;
pub mod llm;
pub mod tool;

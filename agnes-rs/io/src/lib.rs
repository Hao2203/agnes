//! Async I/O seams for the agnes agent (hexagonal architecture).
//!
//! These are the seams hosts implement to give the agent its I/O: model
//! access ([`llm::Llm`]), tools ([`tool::Tool`]), raw HTTP
//! ([`http::HttpTransport`]), the user's out-of-band actions while a run
//! executes ([`input::Input`]), and the durable session log
//! ([`session::SessionLog`]). Seams are async and `Send`; most are also
//! `Sync` — the session log is not, because a session has a single writer.
//! Swapping runtimes means swapping the implementations (see
//! `agnes-transport-reqwest`), never `agnes-core`.

pub mod http;
pub mod input;
pub mod llm;
pub mod session;
pub mod tool;

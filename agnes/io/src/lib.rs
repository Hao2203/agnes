//! Async I/O seams for the agnes agent (hexagonal architecture).
//!
//! These are the seams hosts implement to give the agent its I/O: model
//! access ([`llm::Llm`]), tools ([`tool::Tool`]), raw HTTP
//! ([`http::HttpTransport`]), and the user's out-of-band actions while a run
//! executes ([`input::Input`]). All seams are async and `Send + Sync`, so any
//! async runtime can drive them; swapping runtimes means swapping the
//! implementations (see `agnes-transport-reqwest`), never `agnes-core`.

pub mod http;
pub mod input;
pub mod llm;
pub mod tool;

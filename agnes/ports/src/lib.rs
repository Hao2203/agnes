//! Async ports for the agnes agent (hexagonal architecture).
//!
//! These are the seams hosts implement to give the agent its I/O: model
//! access ([`llm::Llm`]), tools ([`tool::Tool`]), and raw HTTP
//! ([`http::HttpTransport`]). All ports are async and `Send + Sync`, so any
//! async runtime can drive them; swapping runtimes means swapping the
//! implementations (see `agnes-transport-reqwest`), never `agnes-core`.

pub mod http;
pub mod llm;
pub mod tool;

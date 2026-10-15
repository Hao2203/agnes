//! Tiny, dependency-free building blocks with one job: make "empty" states
//! unrepresentable where emptiness is a bug.
//!
//! - [`NonEmpty`]: a collection guaranteed to hold at least one value,
//!   cheap to clone.

mod non_empty;

pub use non_empty::NonEmpty;

use std::fmt;

/// The value that was required to hold something was empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyError;

impl fmt::Display for EmptyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("value must not be empty")
    }
}

impl std::error::Error for EmptyError {}

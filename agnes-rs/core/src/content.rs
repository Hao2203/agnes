//! Non-empty text — a core domain type: goals, model replies and commentary
//! must carry actual content.
//!
//! Content may be as short as a word, so it wraps `ArcColdString`: up to 8
//! bytes are stored inline with no allocation, and longer text is refcounted
//! so a clone never copies. The representation stays internal — the public
//! surface speaks `&str` and `String`.

use agnes_util::EmptyError;
use cold_string::ArcColdString;
use std::fmt;

/// Text guaranteed to hold at least one non-whitespace character. Cheap to
/// clone: short text is stored inline, longer text is refcounted, and a
/// clone never copies either. The stored text is trimmed; validation only
/// rejects strings that are empty or whitespace-only.
#[derive(Clone, PartialEq, Eq)]
pub struct Content(ArcColdString);

impl Content {
    pub fn new<T: AsRef<str>>(value: T) -> Option<Self> {
        let s = value.as_ref().trim();
        if s.is_empty() {
            return None;
        }

        Some(Self(ArcColdString::new(s)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for Content {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl TryFrom<&str> for Content {
    type Error = EmptyError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(EmptyError)
    }
}

impl TryFrom<String> for Content {
    type Error = EmptyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(EmptyError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace_only() {
        assert!(Content::try_from("").is_err());
        assert!(Content::try_from("   \n").is_err());
        assert!(Content::try_from(String::new()).is_err());
    }

    #[test]
    fn builds_from_str_and_string() {
        let from_str = Content::try_from("hello").unwrap();
        let from_string = Content::try_from("hello".to_owned()).unwrap();
        assert_eq!(from_str, from_string);
    }

    #[test]
    fn clone_shares_the_allocation() {
        let original = Content::try_from("a string longer than one machine word").unwrap();
        let clone = original.clone();
        assert_eq!(original.as_str().as_ptr(), clone.as_str().as_ptr());
    }

    #[test]
    fn the_hot_string_representation_stays_one_word() {
        // may-inline strings must keep their representation compact: one
        // machine word, with short text stored inline and no `Option` niche
        // needed.
        assert_eq!(std::mem::size_of::<Content>(), std::mem::size_of::<usize>());
    }
}

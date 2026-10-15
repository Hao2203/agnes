//! Non-empty text — a core domain type: goals, model replies and commentary
//! must carry actual content.

use agnes_util::EmptyError;
use std::fmt;
use std::sync::Arc;

/// Text guaranteed to hold at least one non-whitespace character. Cheap to
/// clone: the text is refcounted, so a clone only bumps the reference count.
#[derive(Clone, PartialEq, Eq)]
pub struct Content(Arc<str>);

impl Content {
    pub fn new<T: AsRef<str>>(value: T) -> Option<Self> {
        let s = value.as_ref().trim();
        if s.is_empty() {
            return None;
        }

        Some(Self(Arc::from(s)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
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
}

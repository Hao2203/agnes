//! Non-empty text — a core domain type: goals, model replies and commentary
//! must carry actual content.

use agnes_util::EmptyError;
use cold_string::ArcColdString;
use std::fmt;

/// Text guaranteed to hold at least one non-whitespace character. Cheap to
/// clone: short strings are stored inline, longer ones are refcounted.
///
/// The stored text is preserved as given (no trimming); validation only
/// rejects strings that are empty or whitespace-only.
#[derive(Clone, PartialEq, Eq)]
pub struct Content(ArcColdString);

impl Content {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for Content {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.0.as_str(), f)
    }
}

impl TryFrom<&str> for Content {
    type Error = EmptyError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.trim().is_empty() {
            return Err(EmptyError);
        }
        Ok(Self(ArcColdString::new(value)))
    }
}

impl TryFrom<String> for Content {
    type Error = EmptyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.trim().is_empty() {
            return Err(EmptyError);
        }
        Ok(Self(ArcColdString::new(value)))
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
    fn preserves_text_verbatim() {
        let content = Content::try_from("  padded  ").unwrap();
        assert_eq!(content.as_str(), "  padded  ");
        assert_eq!(content.to_string(), "  padded  ");
        assert_eq!(format!("{content:?}"), "\"  padded  \"");
    }

    #[test]
    fn clone_shares_the_allocation() {
        // Longer than one word, so the text lives on the heap.
        let original = Content::try_from("a string longer than one machine word").unwrap();
        let clone = original.clone();
        assert_eq!(original.as_str().as_ptr(), clone.as_str().as_ptr());
    }
}

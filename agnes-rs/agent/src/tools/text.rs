//! Shared output limits for built-in tools.

/// Largest tool output handed back to the model, in characters.
pub(crate) const MAX_OUTPUT_CHARS: usize = 100_000;

/// UTF-8 uses at most four bytes per character; reading this many bytes is
/// enough to produce [`MAX_OUTPUT_CHARS`] characters with a little slack.
pub(crate) const MAX_READ_BYTES: u64 = 400_004;

/// Byte budget for assembling a text output before truncation.
pub(crate) const MAX_OUTPUT_BYTES: usize = 400_004;

/// Truncates on a UTF-8 boundary in one scan. Byte length is an upper bound
/// on character count, so the fast path is safe without scanning.
pub(crate) fn truncate(mut text: String) -> String {
    if text.len() <= MAX_OUTPUT_CHARS {
        return text;
    }
    match text.char_indices().nth(MAX_OUTPUT_CHARS) {
        Some((index, _)) => {
            text.truncate(index);
            text.push_str("\n... (output truncated)");
            text
        }
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_unchanged() {
        assert_eq!(truncate("hello".to_owned()), "hello");
    }

    #[test]
    fn multibyte_text_at_the_char_limit_is_not_truncated() {
        let text = "中".repeat(MAX_OUTPUT_CHARS);
        assert_eq!(truncate(text.clone()), text);
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        let text = "中".repeat(MAX_OUTPUT_CHARS + 10);
        let truncated = truncate(text);
        assert!(truncated.ends_with("\n... (output truncated)"));
        assert_eq!(truncated.matches('中').count(), MAX_OUTPUT_CHARS);
    }
}

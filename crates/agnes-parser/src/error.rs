use agnes_ast::Span;
use std::fmt;

/// Parse error with a byte-offset span and human message.
#[derive(Debug, Clone, thiserror::Error)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Parse error at bytes {}..{}: {}",
            self.span.start, self.span.end, self.message
        )
    }
}

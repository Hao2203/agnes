//! The raw HTTP seam, built on the `http` crate's request/response types.
//! Providers stay transport-agnostic: they are generic over
//! [`HttpTransport`], and hosts pick the client — reqwest on tokio today,
//! another stack tomorrow.

use std::fmt;

use async_trait::async_trait;

/// Failure while performing an HTTP request. Payloads carry the underlying
/// error — never a formatted string.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The connection or stream failed at the I/O level.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The request could not be completed for another reason.
    #[error(transparent)]
    Request(#[from] anyhow::Error),
}

/// Sends a request and returns the raw response with *any* status code;
/// deciding what a status means is the provider's job, not the transport's.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn send(
        &self,
        request: http::Request<String>,
    ) -> Result<http::Response<String>, TransportError>;
}

/// Wrapper for logging a request without leaking credentials: header values
/// of `Authorization` are masked, the body is reduced to its length.
pub struct RedactedRequest<'a>(pub &'a http::Request<String>);

impl fmt::Debug for RedactedRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let request = self.0;
        f.debug_struct("Request")
            .field("method", request.method())
            .field("uri", request.uri())
            .field("headers", &redacted_headers(request.headers()))
            .field("body_len", &request.body().len())
            .finish()
    }
}

fn redacted_headers(headers: &http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = if name == http::header::AUTHORIZATION {
                "***".to_owned()
            } else {
                value.to_str().unwrap_or("<non-utf8>").to_owned()
            };
            (name.to_string(), value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_debug_masks_authorization() {
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri("https://example.com/v1/chat/completions")
            .header(http::header::AUTHORIZATION, "Bearer sk-secret")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body("{}".to_owned())
            .unwrap();

        let rendered = format!("{:?}", RedactedRequest(&request));
        assert!(rendered.contains("***"));
        assert!(!rendered.contains("sk-secret"));
        assert!(rendered.contains("application/json"));
        assert!(rendered.contains("body_len: 2"));
    }
}

//! The raw HTTP seam, built on the `http` crate's request/response types.
//! Providers stay transport-agnostic: they are generic over
//! [`HttpTransport`], and hosts pick the client — reqwest on tokio today,
//! another stack tomorrow.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

/// Failure while performing an HTTP request. Payloads carry the underlying
/// error — never a formatted string.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The connection or stream failed at the I/O level.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The request exceeded its configured deadline.
    #[error("request timed out")]
    Timeout,
    /// The request was cancelled before it completed (host shutdown or an
    /// explicit cancellation). Callers treat it as an interruption, not a
    /// failure.
    #[error("request was cancelled")]
    Cancelled,
    /// The response body was not valid UTF-8. The transport never replaces
    /// invalid sequences silently.
    #[error("response body is not valid UTF-8")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    /// The response body exceeded the transport's safety limit.
    #[error("response body exceeds {limit} bytes")]
    BodyTooLarge { limit: usize },
    /// The request could not be built by the `http` crate.
    #[error("http request could not be built")]
    Request(#[source] http::Error),
    /// The client reported another failure (TLS, resolver, redirect, …).
    #[error("{0}")]
    Other(#[source] Box<dyn std::error::Error + Send + Sync>),
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

#[async_trait]
impl<T: HttpTransport + ?Sized> HttpTransport for Arc<T> {
    async fn send(
        &self,
        request: http::Request<String>,
    ) -> Result<http::Response<String>, TransportError> {
        (**self).send(request).await
    }
}

#[async_trait]
impl<T: HttpTransport + ?Sized> HttpTransport for Box<T> {
    async fn send(
        &self,
        request: http::Request<String>,
    ) -> Result<http::Response<String>, TransportError> {
        (**self).send(request).await
    }
}

/// The installed HTTP transport as a shareable handle.
///
/// Plugins mount one implementation into the world under this type;
/// provider plugins read it at build time to wire their clients. The type
/// is plain data — it becomes a resource only where an ECS installs it.
#[derive(Clone)]
pub struct HttpTransportResource(Arc<dyn HttpTransport>);

impl HttpTransportResource {
    /// Wraps a transport implementation.
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self(transport)
    }

    /// The installed transport.
    pub fn transport(&self) -> &Arc<dyn HttpTransport> {
        &self.0
    }
}

/// Wrapper for logging a request without leaking credentials: only
/// `Content-Type`, `Accept`, and `User-Agent` stay visible; every other header
/// value is masked and the body is reduced to its length.
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
    const VISIBLE: [http::header::HeaderName; 3] = [
        http::header::CONTENT_TYPE,
        http::header::ACCEPT,
        http::header::USER_AGENT,
    ];

    let mut rendered = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let value = if VISIBLE.contains(name) {
            value.to_str().unwrap_or("<non-utf8>").to_owned()
        } else {
            "***".to_owned()
        };
        rendered.push((name.to_string(), value));
    }
    rendered
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
            .header(http::header::COOKIE, "session=secret")
            .header("x-api-key", "secret-key")
            .body("{}".to_owned())
            .unwrap();

        let rendered = format!("{:?}", RedactedRequest(&request));
        assert!(rendered.contains("***"));
        assert!(!rendered.contains("sk-secret"));
        assert!(!rendered.contains("session=secret"));
        assert!(!rendered.contains("secret-key"));
        assert!(rendered.contains("application/json"));
        assert!(rendered.contains("body_len: 2"));
    }
}

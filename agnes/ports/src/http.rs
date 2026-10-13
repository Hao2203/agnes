//! The raw HTTP seam. Providers stay transport-agnostic: they are generic
//! over [`HttpTransport`], and hosts pick the client — reqwest on tokio
//! today, another stack tomorrow.

use std::fmt;

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

/// Minimal HTTP request description. `body` carries the JSON document
/// (empty for GET).
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

fn redacted<'a>(name: &str, value: &'a str) -> &'a str {
    if name.eq_ignore_ascii_case("authorization") {
        "***"
    } else {
        value
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), redacted(k, v)))
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection error: {0}")]
    Io(String),
    #[error("request could not be sent: {0}")]
    Request(String),
}

/// Sends a request and returns the raw response with *any* status code;
/// deciding what a status means is the provider's job, not the transport's.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_debug_redacts_authorization() {
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "https://example.com".to_owned(),
            headers: vec![
                ("Authorization".to_owned(), "Bearer sk-secret".to_owned()),
                ("Content-Type".to_owned(), "application/json".to_owned()),
            ],
            body: "{}".to_owned(),
        };
        let rendered = format!("{req:?}");
        assert!(rendered.contains("***"));
        assert!(!rendered.contains("sk-secret"));
        assert!(rendered.contains("application/json"));
    }

    #[test]
    fn success_range() {
        assert!(
            HttpResponse {
                status: 200,
                body: String::new()
            }
            .is_success()
        );
        assert!(
            !HttpResponse {
                status: 401,
                body: String::new()
            }
            .is_success()
        );
    }
}

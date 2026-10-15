//! Async HTTP transport on reqwest (tokio). The default transport for the
//! CLI; swap the crate to move to another runtime.

use std::time::Duration;

use agnes_io::http::{HttpTransport, TransportError};
use async_trait::async_trait;

/// Async transport with per-request total and connect timeouts.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client configuration is static and valid");
        Self { client }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new(Duration::from_secs(300))
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(
        &self,
        request: http::Request<String>,
    ) -> Result<http::Response<String>, TransportError> {
        let (parts, body) = request.into_parts();
        let mut builder = self
            .client
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers);
        if !body.is_empty() {
            builder = builder.body(body);
        }
        let response = builder.send().await.map_err(transport_error)?;

        let mut builder = http::Response::builder().status(response.status());
        if let Some(headers) = builder.headers_mut() {
            *headers = response.headers().clone();
        }
        let body = response.text().await.map_err(transport_error)?;
        builder.body(body).map_err(transport_error_from_http)
    }
}

/// Classifies a reqwest failure: connection- and body-level problems map to
/// [`TransportError::Io`], the rest keep their full error chain in
/// [`TransportError::Request`].
fn transport_error(error: reqwest::Error) -> TransportError {
    let io_like = error.is_connect() || error.is_timeout() || error.is_body() || error.is_decode();
    if io_like {
        TransportError::Io(std::io::Error::other(error))
    } else {
        TransportError::Request(anyhow::Error::new(error))
    }
}

fn transport_error_from_http(error: http::Error) -> TransportError {
    TransportError::Request(anyhow::Error::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves exactly one response and returns the request that arrived.
    fn serve_once(
        listener: std::net::TcpListener,
        response: &'static str,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).expect("read request");
                assert!(n > 0, "client closed early");
                buf.extend_from_slice(&chunk[..n]);
                let Some(headers_end) = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|pos| pos + 4)
                else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..headers_end]).to_string();
                let content_length: usize = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().expect("content-length"))
                    })
                    .unwrap_or(0);
                if buf.len() >= headers_end + content_length {
                    let body =
                        String::from_utf8_lossy(&buf[headers_end..headers_end + content_length])
                            .into_owned();
                    stream
                        .write_all(response.as_bytes())
                        .expect("write response");
                    return body;
                }
            }
        })
    }

    fn post(url: String, body: &str) -> http::Request<String> {
        http::Request::builder()
            .method(http::Method::POST)
            .uri(url)
            .header(http::header::AUTHORIZATION, "Bearer sk-test")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(body.to_owned())
            .unwrap()
    }

    /// Test transports bypass environment proxies so results are hermetic;
    /// the production constructors keep honoring `HTTP(S)_PROXY`.
    fn test_transport() -> ReqwestTransport {
        ReqwestTransport {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        }
    }

    #[tokio::test]
    async fn posts_body_and_returns_status_and_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let server = serve_once(
            listener,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\nok",
        );

        let response = test_transport()
            .send(post(url, r#"{"model":"gpt-test"}"#))
            .await
            .unwrap();

        let request_body = server.join().unwrap();
        assert!(request_body.contains(r#"{"model":"gpt-test"}"#));
        assert!(response.status().is_success());
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.body(), "ok");
    }

    #[tokio::test]
    async fn returns_error_statuses_untouched() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let server = serve_once(
            listener,
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 19\r\n\r\n{\"error\":\"bad key\"}",
        );

        let response = test_transport().send(post(url, "{}")).await.unwrap();

        server.join().unwrap();
        assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
        assert!(response.body().contains("bad key"));
    }

    #[tokio::test]
    async fn connection_failure_is_an_io_error() {
        // Port 1 on localhost refuses connections.
        let err = test_transport()
            .send(post(
                "http://127.0.0.1:1/v1/chat/completions".to_owned(),
                "{}",
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Io(_)));
    }
}

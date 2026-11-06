//! Async HTTP transport on reqwest (tokio). The default transport for the
//! CLI; swap the crate to move to another runtime.

use std::time::Duration;

use agnes_io::http::{HttpStreamPart, HttpTransport, TransportError};
use async_trait::async_trait;

/// Hard cap on one response body; a runaway or hostile peer must not be able
/// to exhaust memory before the provider sees the payload.
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// Async transport with per-request total and connect timeouts.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Builds a client with the given total timeout and a fixed 15-second
    /// connect timeout.
    ///
    /// # Errors
    ///
    /// Returns the client build error when the TLS backend, root
    /// certificates, or resolver configuration cannot be initialized.
    pub fn new(timeout: Duration) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(transport_error)?;
        Ok(Self { client })
    }
}

impl ReqwestTransport {
    fn begin(&self, request: http::Request<String>) -> reqwest::RequestBuilder {
        let (parts, body) = request.into_parts();
        let builder = self
            .client
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers);
        if body.is_empty() {
            builder
        } else {
            builder.body(body)
        }
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(
        &self,
        request: http::Request<String>,
    ) -> Result<http::Response<String>, TransportError> {
        let mut response = self.begin(request).send().await.map_err(transport_error)?;
        if let Some(length) = response.content_length()
            && length > MAX_BODY_BYTES
        {
            return Err(body_too_large());
        }

        let status = response.status();
        let headers = response.headers().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or_else(body_too_large)?;
            if u64::try_from(next_len).unwrap_or(u64::MAX) > MAX_BODY_BYTES {
                return Err(body_too_large());
            }
            bytes.extend_from_slice(&chunk);
        }

        let mut builder = http::Response::builder().status(status);
        if let Some(out) = builder.headers_mut() {
            *out = headers;
        }
        let body = std::str::from_utf8(&bytes)?.to_owned();
        builder.body(body).map_err(TransportError::Request)
    }

    async fn send_stream(
        &self,
        request: http::Request<String>,
        sink: &mut (dyn FnMut(HttpStreamPart) + Send),
    ) -> Result<(), TransportError> {
        let mut response = self.begin(request).send().await.map_err(transport_error)?;
        if let Some(length) = response.content_length()
            && length > MAX_BODY_BYTES
        {
            return Err(body_too_large());
        }

        sink(HttpStreamPart::Head {
            status: response.status(),
            headers: response.headers().clone(),
        });

        let mut total = 0_u64;
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            total = total.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
            if total > MAX_BODY_BYTES {
                return Err(body_too_large());
            }
            sink(HttpStreamPart::Body(chunk));
        }
        Ok(())
    }
}

fn body_too_large() -> TransportError {
    TransportError::BodyTooLarge {
        limit: usize::try_from(MAX_BODY_BYTES).unwrap_or(usize::MAX),
    }
}

/// Classifies a reqwest failure without flattening it into a string.
fn transport_error(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else if error.is_connect() || error.is_body() || error.is_decode() {
        TransportError::Io(std::io::Error::other(error))
    } else {
        TransportError::Other(error.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves exactly one response and returns the request that arrived.
    fn serve_once(
        listener: std::net::TcpListener,
        response: Vec<u8>,
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
                    stream.write_all(&response).expect("write response");
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
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\nok"
                .to_vec(),
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
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 19\r\n\r\n{\"error\":\"bad key\"}"
                .to_vec(),
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

    #[tokio::test]
    async fn invalid_utf8_is_rejected() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let server = serve_once(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n\xff\xfe".to_vec(),
        );

        let err = test_transport().send(post(url, "{}")).await.unwrap_err();

        server.join().unwrap();
        assert!(matches!(err, TransportError::InvalidUtf8(_)));
    }

    /// Serves one response and closes, without a content length.
    fn serve_raw(
        listener: std::net::TcpListener,
        response: Vec<u8>,
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
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end + 4]).to_string();
                    let content_length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().expect("content-length"))
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + content_length {
                        stream.write_all(&response).expect("write response");
                        return String::from_utf8_lossy(&buf[end + 4..]).into_owned();
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn a_streamed_response_delivers_the_head_and_body_parts() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let server = serve_raw(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
              data: one\n\ndata: two\n\n"
                .to_vec(),
        );

        let mut parts = Vec::new();
        test_transport()
            .send_stream(post(url, "{}"), &mut |part| parts.push(part))
            .await
            .unwrap();

        server.join().unwrap();
        assert!(matches!(
            parts.first(),
            Some(HttpStreamPart::Head { status, .. }) if *status == http::StatusCode::OK
        ));
        let body: Vec<u8> = parts
            .iter()
            .filter_map(|part| match part {
                HttpStreamPart::Body(bytes) => Some(bytes.as_ref()),
                HttpStreamPart::Head { .. } => None,
            })
            .flatten()
            .copied()
            .collect();
        assert_eq!(body, b"data: one\n\ndata: two\n\n");
    }

    #[tokio::test]
    async fn an_oversized_content_length_is_refused_before_reading() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        )
        .into_bytes();
        let server = serve_once(listener, response);

        let err = test_transport().send(post(url, "{}")).await.unwrap_err();

        server.join().unwrap();
        assert!(matches!(err, TransportError::BodyTooLarge { .. }));
    }
}

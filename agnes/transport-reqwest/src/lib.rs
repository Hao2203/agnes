//! Async HTTP transport on reqwest (tokio). The default transport for the
//! CLI; swap the crate to move to another runtime.

use std::time::Duration;

use agnes_ports::http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, TransportError};
use async_trait::async_trait;

/// Blocking-free transport with per-request total and connect timeouts.
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
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
        };
        let mut req = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            req = req.header(name, value);
        }
        if request.method == HttpMethod::Post {
            req = req.body(request.body);
        }
        let response = req
            .send()
            .await
            .map_err(|e| TransportError::Io(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| TransportError::Io(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
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

        let transport = ReqwestTransport::new(Duration::from_secs(10));
        let response = transport
            .send(HttpRequest {
                method: HttpMethod::Post,
                url,
                headers: vec![
                    ("Authorization".to_owned(), "Bearer sk-test".to_owned()),
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                ],
                body: r#"{"model":"gpt-test"}"#.to_owned(),
            })
            .await
            .unwrap();

        let request_body = server.join().unwrap();
        assert!(request_body.contains(r#"{"model":"gpt-test"}"#));
        assert!(response.is_success());
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "ok");
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

        let transport = ReqwestTransport::default();
        let response = transport
            .send(HttpRequest {
                method: HttpMethod::Post,
                url,
                headers: vec![],
                body: "{}".to_owned(),
            })
            .await
            .unwrap();

        server.join().unwrap();
        assert_eq!(response.status, 401);
        assert!(response.body.contains("bad key"));
    }
}

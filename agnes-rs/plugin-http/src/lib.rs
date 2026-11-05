//! ECS-native HTTP capability.
//!
//! [`HttpPlugin`] owns the request lifecycle, not just the transport: systems
//! submit [`HttpRequest`] messages carrying an id from [`HttpClient`], the
//! plugin performs each request on the async executor, and the outcome arrives
//! as an [`HttpResponse`] message with the same id. [`HttpCancel`] stops an
//! in-flight request (best effort — a cancellation always answers with
//! [`TransportError::Cancelled`], so callers are never left waiting).
//!
//! In-flight requests live in [`HttpInFlight`] and are cancelled when the app
//! shuts down. The plugin mounts the transport implementation as
//! [`HttpTransportResource`] for its own dispatch system; consumers never
//! touch the transport directly.
//!
//! Plugins apply immediately in registration order: add this plugin **before**
//! any plugin that submits requests (for example `agnes-plugin-openai`),
//! because those plugins check for [`HttpClient`] when they build.
//!
//! # Example
//!
//! ```ignore
//! fn submit(mut client: ResMut<HttpClient>, mut requests: MessageWriter<HttpRequest>) {
//!     let id = client.request_id();
//!     requests.write(HttpRequest { id, request: build_request() });
//! }
//!
//! fn receive(mut responses: MessageReader<HttpResponse>) {
//!     for response in responses.read() {
//!         // correlate with the id chosen above
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use agnes_ecs::{App, CancelToken, MessageReader, Plugin, Res, ResMut, Update};
use agnes_io::http::{HttpTransport, HttpTransportResource, TransportError};
use agnes_plugin_async::AsyncExecutor;

/// The identity of one HTTP request.
///
/// Responses carry the same id back to the submitter; [`HttpClient`] never
/// hands out an id twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HttpRequestId(u64);

impl HttpRequestId {
    /// Wraps a raw number (hosts that mint their own ids).
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw number.
    pub fn value(&self) -> u64 {
        self.0
    }
}

/// The request id issuer; also marks that the HTTP capability is installed.
///
/// Provider plugins check for this resource when they build, so forgetting
/// [`HttpPlugin`] fails loudly instead of letting requests disappear.
#[derive(Default)]
pub struct HttpClient {
    next: u64,
}

impl HttpClient {
    /// Issues the next id.
    ///
    /// # Panics
    ///
    /// Panics when the id space is exhausted.
    pub fn request_id(&mut self) -> HttpRequestId {
        let id = HttpRequestId(self.next);
        self.next = self
            .next
            .checked_add(1)
            .expect("http request id space exhausted");
        id
    }
}

/// One request to perform.
#[derive(Debug)]
pub struct HttpRequest {
    pub id: HttpRequestId,
    pub request: http::Request<String>,
}

/// The outcome of one request: a response with any status code, or a
/// transport failure (including [`TransportError::Cancelled`]).
#[derive(Debug)]
pub struct HttpResponse {
    pub id: HttpRequestId,
    pub result: Result<http::Response<String>, TransportError>,
}

/// Stops an in-flight request (best effort).
#[derive(Clone, Copy, Debug)]
pub struct HttpCancel {
    pub id: HttpRequestId,
}

/// The requests currently in flight, keyed by id.
#[derive(Default)]
pub struct HttpInFlight(HashMap<HttpRequestId, CancelToken>);

impl HttpInFlight {
    /// The number of requests in flight.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no request is in flight.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The HTTP capability: dispatch, cancellation, and in-flight bookkeeping.
///
/// The transport implementation itself lives outside (today
/// `agnes-transport-reqwest`); this plugin only mounts it and drives it.
pub struct HttpPlugin {
    transport: Arc<dyn HttpTransport>,
}

impl HttpPlugin {
    /// Creates the plugin from a transport implementation.
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }
}

impl Plugin for HttpPlugin {
    fn name(&self) -> &'static str {
        "agnes-http"
    }

    fn build(&self, app: &mut App) {
        app.insert_resource(HttpTransportResource::new(Arc::clone(&self.transport)))
            .insert_resource(HttpClient::default())
            .insert_resource(HttpInFlight::default())
            .add_systems(Update, dispatch_http)
            .add_systems(Update, cancel_http)
            .add_systems(Update, reap_http);
    }
}

/// Performs submitted requests on the async executor; the outcome comes back
/// as an [`HttpResponse`] host message.
fn dispatch_http(
    executor: Res<AsyncExecutor>,
    transport: Res<HttpTransportResource>,
    mut in_flight: ResMut<HttpInFlight>,
    mut requests: MessageReader<HttpRequest>,
) {
    for message in requests.read() {
        let id = message.id;
        let request = message.request.clone();
        let token = CancelToken::new();
        in_flight.0.insert(id, token.clone());
        let transport = Arc::clone(transport.transport());
        let shutdown = executor.cancellation();
        let sender = executor.sender();
        let task = async move {
            let result = tokio::select! {
                biased;
                () = token.cancelled() => Err(TransportError::Cancelled),
                () = shutdown.cancelled() => Err(TransportError::Cancelled),
                result = transport.send(request) => result,
            };
            let _ = sender.send_message(HttpResponse { id, result });
        };
        if let Err(error) = executor.spawn(task) {
            in_flight.0.remove(&id);
            let _ = executor.sender().send_message(HttpResponse {
                id,
                result: Err(TransportError::Other(Box::new(error))),
            });
        }
    }
}

/// Cancels in-flight requests; unknown ids are ignored.
fn cancel_http(mut cancels: MessageReader<HttpCancel>, in_flight: ResMut<HttpInFlight>) {
    for message in cancels.read() {
        if let Some(token) = in_flight.0.get(&message.id) {
            token.cancel();
        }
    }
}

/// Drops finished requests from the in-flight table.
fn reap_http(mut responses: MessageReader<HttpResponse>, mut in_flight: ResMut<HttpInFlight>) {
    for message in responses.read() {
        in_flight.0.remove(&message.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_plugin_async::TokioPlugin;
    use async_trait::async_trait;

    struct OkTransport;

    #[async_trait]
    impl HttpTransport for OkTransport {
        async fn send(
            &self,
            _request: http::Request<String>,
        ) -> Result<http::Response<String>, TransportError> {
            Ok(http::Response::builder()
                .status(200)
                .body("body".to_owned())
                .expect("static response is valid"))
        }
    }

    struct FailingTransport;

    #[async_trait]
    impl HttpTransport for FailingTransport {
        async fn send(
            &self,
            _request: http::Request<String>,
        ) -> Result<http::Response<String>, TransportError> {
            Err(TransportError::Timeout)
        }
    }

    struct HangingTransport;

    #[async_trait]
    impl HttpTransport for HangingTransport {
        async fn send(
            &self,
            _request: http::Request<String>,
        ) -> Result<http::Response<String>, TransportError> {
            std::future::pending().await
        }
    }

    #[derive(Default, Clone)]
    struct Captured {
        outcomes: Vec<Result<u16, String>>,
        in_flight: usize,
    }

    fn capture(
        mut responses: MessageReader<HttpResponse>,
        in_flight: Res<HttpInFlight>,
        mut captured: ResMut<Captured>,
    ) {
        for message in responses.read() {
            captured.outcomes.push(match &message.result {
                Ok(response) => Ok(response.status().as_u16()),
                Err(error) => Err(error.to_string()),
            });
        }
        captured.in_flight = in_flight.len();
    }

    fn app(transport: Arc<dyn HttpTransport>) -> App {
        let mut app = App::new();
        app.add_plugin(HttpPlugin::new(transport))
            .insert_resource(Captured::default())
            .add_systems(Update, capture)
            .add_plugin(TokioPlugin);
        app
    }

    fn captured(app: &App) -> Captured {
        app.world()
            .resource::<Captured>()
            .expect("captured is installed")
            .clone()
    }

    fn send(app: &mut App, id: u64) {
        app.world_mut().send_message(HttpRequest {
            id: HttpRequestId::new(id),
            request: http::Request::builder()
                .uri("https://example.com")
                .body(String::new())
                .expect("static request is valid"),
        });
    }

    async fn wait_for(app: &mut App, count: usize) -> Captured {
        for _ in 0..256 {
            app.pump().expect("schedule runs");
            tokio::task::yield_now().await;
            let snapshot = captured(app);
            if snapshot.outcomes.len() >= count {
                return snapshot;
            }
        }
        panic!("no response after 256 pumps");
    }

    #[tokio::test]
    async fn a_request_answers_with_the_transport_response() {
        let mut app = app(Arc::new(OkTransport));
        send(&mut app, 1);

        let snapshot = wait_for(&mut app, 1).await;
        assert_eq!(snapshot.outcomes, vec![Ok(200)]);
        assert_eq!(snapshot.in_flight, 0);
    }

    #[tokio::test]
    async fn transport_failures_are_forwarded() {
        let mut app = app(Arc::new(FailingTransport));
        send(&mut app, 1);

        let snapshot = wait_for(&mut app, 1).await;
        assert!(matches!(&snapshot.outcomes[..], [Err(message)] if message.contains("timed out")));
        assert_eq!(snapshot.in_flight, 0);
    }

    #[tokio::test]
    async fn a_cancel_answers_cancelled() {
        let mut app = app(Arc::new(HangingTransport));
        send(&mut app, 7);
        app.pump().expect("dispatch runs");
        app.world_mut().send_message(HttpCancel {
            id: HttpRequestId::new(7),
        });

        let snapshot = wait_for(&mut app, 1).await;
        assert!(matches!(&snapshot.outcomes[..], [Err(message)] if message.contains("cancelled")));
        assert_eq!(snapshot.in_flight, 0);
    }

    #[test]
    fn ids_are_issued_monotonically() {
        let mut client = HttpClient::default();
        assert_eq!(client.request_id(), HttpRequestId::new(0));
        assert_eq!(client.request_id(), HttpRequestId::new(1));
    }
}

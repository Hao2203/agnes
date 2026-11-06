//! ECS-native HTTP capability.
//!
//! [`HttpPlugin`] owns the request lifecycle, not just the transport: systems
//! submit [`HttpRequest`] messages carrying an id from [`HttpClient`], the
//! plugin performs each request on the async executor, and the outcome arrives
//! as an [`HttpResponse`] message with the same id. [`HttpCancel`] stops an
//! in-flight request (best effort — a cancellation always answers with
//! [`TransportError::Cancelled`], so callers are never left waiting).
//!
//! Each in-flight request is an entity carrying the [`HttpInFlight`] marker,
//! its [`HttpRequestId`], and a [`CancelToken`] — one entity per request,
//! spawned at dispatch and despawned when the outcome arrives, so the
//! in-flight table can never leak an entry. Everything is cancelled when the
//! app shuts down. The plugin mounts the transport implementation as
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

use std::sync::Arc;

use agnes_ecs::{
    App, CancelToken, Commands, Entity, MessageReader, Plugin, Query, Res, Update, With,
};
use agnes_io::http::{HttpStreamPart, HttpTransport, HttpTransportResource, TransportError};
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

/// One request whose response is consumed incrementally.
///
/// The parts arrive as [`HttpStreamEvent`] messages and the call closes with
/// [`HttpStreamEnd`]; no [`HttpResponse`] is produced for it.
#[derive(Debug)]
pub struct HttpStreamRequest {
    pub id: HttpRequestId,
    pub request: http::Request<String>,
}

/// One part of a streamed response, in arrival order. The head carries the
/// status and headers; body parts follow.
#[derive(Debug)]
pub struct HttpStreamEvent {
    pub id: HttpRequestId,
    pub part: HttpStreamPart,
}

/// A streamed response ended; the result is the transport outcome, not an
/// HTTP status (the status arrived with the head).
#[derive(Debug)]
pub struct HttpStreamEnd {
    pub id: HttpRequestId,
    pub result: Result<(), TransportError>,
}

/// Stops an in-flight request (best effort).
#[derive(Clone, Copy, Debug)]
pub struct HttpCancel {
    pub id: HttpRequestId,
}

/// Marker for an in-flight request entity.
///
/// One entity per submitted request, carrying the [`HttpInFlight`] marker,
/// the request's [`HttpRequestId`], and the [`CancelToken`] that stops it.
/// Spawned by the dispatch systems and despawned by the reaper once the
/// outcome (response, stream end, or transport failure) has been delivered,
/// so a finished request can never linger.
pub struct HttpInFlight;

agnes_ecs::component!(HttpInFlight, HttpRequestId);

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
            .add_systems(Update, dispatch_http)
            .add_systems(Update, dispatch_http_streams)
            .add_systems(Update, cancel_http)
            .add_systems(Update, reap_http);
    }
}

/// Performs submitted requests on the async executor; the outcome comes back
/// as an [`HttpResponse`] host message.
fn dispatch_http(
    executor: Res<AsyncExecutor>,
    transport: Res<HttpTransportResource>,
    mut requests: MessageReader<HttpRequest>,
    mut commands: Commands,
) {
    for message in requests.read() {
        let id = message.id;
        let request = message.request.clone();
        let token = CancelToken::new();
        let cancel = token.clone();
        let transport = Arc::clone(transport.transport());
        let shutdown = executor.cancellation();
        let sender = executor.sender();
        let task = async move {
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => Err(TransportError::Cancelled),
                () = shutdown.cancelled() => Err(TransportError::Cancelled),
                result = transport.send(request) => result,
            };
            let _ = sender.send_message(HttpResponse { id, result });
        };
        match executor.spawn(task) {
            Ok(()) => {
                commands.spawn_bundle((HttpInFlight, id, token));
            }
            Err(error) => {
                let _ = executor.sender().send_message(HttpResponse {
                    id,
                    result: Err(TransportError::Other(error.into())),
                });
            }
        };
    }
}

/// Performs submitted streaming requests on the async executor; parts arrive
/// as [`HttpStreamEvent`] host messages and the call closes with
/// [`HttpStreamEnd`].
fn dispatch_http_streams(
    executor: Res<AsyncExecutor>,
    transport: Res<HttpTransportResource>,
    mut requests: MessageReader<HttpStreamRequest>,
    mut commands: Commands,
) {
    for message in requests.read() {
        let id = message.id;
        let request = message.request.clone();
        let token = CancelToken::new();
        let cancel = token.clone();
        let transport = Arc::clone(transport.transport());
        let shutdown = executor.cancellation();
        let sender = executor.sender();
        let task = async move {
            let mut sink = |part: HttpStreamPart| {
                let _ = sender.send_message(HttpStreamEvent { id, part });
            };
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => Err(TransportError::Cancelled),
                () = shutdown.cancelled() => Err(TransportError::Cancelled),
                result = transport.send_stream(request, &mut sink) => result,
            };
            let _ = sender.send_message(HttpStreamEnd { id, result });
        };
        match executor.spawn(task) {
            Ok(()) => {
                commands.spawn_bundle((HttpInFlight, id, token));
            }
            Err(error) => {
                let _ = executor.sender().send_message(HttpStreamEnd {
                    id,
                    result: Err(TransportError::Other(error.into())),
                });
            }
        };
    }
}

/// Cancels in-flight requests; unknown ids are ignored.
fn cancel_http(
    mut cancels: MessageReader<HttpCancel>,
    in_flight: Query<(&HttpRequestId, &CancelToken), With<HttpInFlight>>,
) {
    for message in cancels.read() {
        for (id, token) in in_flight.iter() {
            if *id == message.id {
                token.cancel();
            }
        }
    }
}

/// Despawns the entities of finished requests so the in-flight table can
/// never leak an entry.
fn reap_http(
    mut responses: MessageReader<HttpResponse>,
    mut ends: MessageReader<HttpStreamEnd>,
    in_flight: Query<(Entity, &HttpRequestId), With<HttpInFlight>>,
    mut commands: Commands,
) {
    let finished: Vec<HttpRequestId> = responses
        .read()
        .iter()
        .map(|message| message.id)
        .chain(ends.read().iter().map(|message| message.id))
        .collect();
    for (entity, id) in in_flight.iter() {
        if finished.contains(&id) {
            commands.entity(entity).despawn();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_ecs::ResMut;
    use agnes_plugin_async::TokioPlugin;
    use async_trait::async_trait;
    use bytes::Bytes;

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

    struct StreamingTransport;

    #[async_trait]
    impl HttpTransport for StreamingTransport {
        async fn send(
            &self,
            _request: http::Request<String>,
        ) -> Result<http::Response<String>, TransportError> {
            Ok(http::Response::builder()
                .status(200)
                .body("buffered".to_owned())
                .expect("static response is valid"))
        }

        async fn send_stream(
            &self,
            _request: http::Request<String>,
            sink: &mut (dyn FnMut(HttpStreamPart) + Send),
        ) -> Result<(), TransportError> {
            sink(HttpStreamPart::Head {
                status: http::StatusCode::OK,
                headers: http::HeaderMap::new(),
            });
            sink(HttpStreamPart::Body(Bytes::from_static(b"one")));
            sink(HttpStreamPart::Body(Bytes::from_static(b"two")));
            Ok(())
        }
    }

    struct HangingStream;

    #[async_trait]
    impl HttpTransport for HangingStream {
        async fn send(
            &self,
            _request: http::Request<String>,
        ) -> Result<http::Response<String>, TransportError> {
            std::future::pending().await
        }

        async fn send_stream(
            &self,
            _request: http::Request<String>,
            _sink: &mut (dyn FnMut(HttpStreamPart) + Send),
        ) -> Result<(), TransportError> {
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
        in_flight: Query<Entity, With<HttpInFlight>>,
        mut captured: ResMut<Captured>,
    ) {
        for message in responses.read() {
            captured.outcomes.push(match &message.result {
                Ok(response) => Ok(response.status().as_u16()),
                Err(error) => Err(error.to_string()),
            });
        }
        captured.in_flight = in_flight.iter().count();
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

    #[derive(Default, Clone)]
    struct CapturedStream {
        parts: Vec<String>,
        ended: Vec<Result<(), String>>,
        in_flight: usize,
    }

    fn capture_stream(
        mut events: MessageReader<HttpStreamEvent>,
        mut ends: MessageReader<HttpStreamEnd>,
        in_flight: Query<Entity, With<HttpInFlight>>,
        mut captured: ResMut<CapturedStream>,
    ) {
        for event in events.read() {
            captured.parts.push(match &event.part {
                HttpStreamPart::Head { status, .. } => format!("head:{}", status.as_u16()),
                HttpStreamPart::Body(bytes) => {
                    format!("body:{}", String::from_utf8_lossy(bytes))
                }
            });
        }
        for end in ends.read() {
            captured.ended.push(match &end.result {
                Ok(()) => Ok(()),
                Err(error) => Err(error.to_string()),
            });
        }
        captured.in_flight = in_flight.iter().count();
    }

    fn stream_app(transport: Arc<dyn HttpTransport>) -> App {
        let mut app = App::new();
        app.add_plugin(HttpPlugin::new(transport))
            .insert_resource(CapturedStream::default())
            .add_systems(Update, capture_stream)
            .add_plugin(TokioPlugin);
        app
    }

    fn send_stream(app: &mut App, id: u64) {
        app.world_mut().send_message(HttpStreamRequest {
            id: HttpRequestId::new(id),
            request: http::Request::builder()
                .uri("https://example.com")
                .body(String::new())
                .expect("static request is valid"),
        });
    }

    async fn wait_for_stream(app: &mut App) -> CapturedStream {
        for _ in 0..256 {
            app.pump().expect("schedule runs");
            tokio::task::yield_now().await;
            let snapshot = app
                .world()
                .resource::<CapturedStream>()
                .expect("captured stream is installed")
                .clone();
            if !snapshot.ended.is_empty() {
                return snapshot;
            }
        }
        panic!("no stream end after 256 pumps");
    }

    #[tokio::test]
    async fn a_streamed_request_answers_with_parts_then_end() {
        let mut app = stream_app(Arc::new(StreamingTransport));
        send_stream(&mut app, 3);

        let snapshot = wait_for_stream(&mut app).await;
        assert_eq!(
            snapshot.parts,
            vec!["head:200", "body:one", "body:two"],
            "the head arrives before the body parts"
        );
        assert_eq!(snapshot.ended, vec![Ok(())]);
        assert_eq!(snapshot.in_flight, 0);
    }

    #[tokio::test]
    async fn a_stream_cancel_answers_cancelled() {
        let mut app = stream_app(Arc::new(HangingStream));
        send_stream(&mut app, 9);
        app.pump().expect("dispatch runs");
        app.world_mut().send_message(HttpCancel {
            id: HttpRequestId::new(9),
        });

        let snapshot = wait_for_stream(&mut app).await;
        assert!(
            matches!(&snapshot.ended[..], [Err(message)] if message.contains("cancelled")),
            "the stream ends with a cancellation"
        );
        assert_eq!(snapshot.in_flight, 0);
    }

    #[test]
    fn ids_are_issued_monotonically() {
        let mut client = HttpClient::default();
        assert_eq!(client.request_id(), HttpRequestId::new(0));
        assert_eq!(client.request_id(), HttpRequestId::new(1));
    }
}

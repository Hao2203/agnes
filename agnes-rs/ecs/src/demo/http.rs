//! Demo HTTP request capability built entirely on messages, components, and
//! plain threads.
//!
//! Like the rest of `demo`, this is a reference implementation: the core ECS
//! knows nothing about HTTP. Requests are ordinary messages, their observable
//! state is an ordinary component, and results come back as messages. The
//! blocking [`HttpClient`] trait is implemented by the host — or by the
//! `http_request` example, which uses `ureq` from `dev-dependencies`.
//!
//! Execution model: each request runs on its own [`std::thread`], so no async
//! runtime is involved at all; [`RuntimeSender`](crate::RuntimeSender) sends
//! the result back synchronously.

use std::sync::Arc;
use std::thread;

use crate::app::{App, Update};
use crate::commands::Commands;
use crate::entity::Entity;
use crate::message::MessageReader;
use crate::plugin::Plugin;
use crate::query::Query;
use crate::resource::{Res, ResMut};
use crate::runner::AsyncExecutor;

/// An HTTP method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl HttpMethod {
    /// The method name as used in a request line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }
}

/// A request description.
#[derive(Clone, Debug)]
pub struct HttpRequestSpec {
    /// The method.
    pub method: HttpMethod,
    /// The full URL.
    pub url: String,
    /// Header pairs.
    pub headers: Vec<(String, String)>,
    /// Optional request body.
    pub body: Option<String>,
}

impl HttpRequestSpec {
    /// Builds a GET request.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// Builds a POST request with a body.
    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Post,
            url: url.into(),
            headers: Vec::new(),
            body: Some(body.into()),
        }
    }
}

/// A completed response. Non-2xx status codes are still a response, matching
/// the `ehttp` convention that only transport failures are errors.
#[derive(Clone, Debug)]
pub struct HttpResponseSpec {
    /// The HTTP status code.
    pub status: u16,
    /// The response body.
    pub body: String,
}

/// A transport-level failure.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// The request could not be completed.
    #[error("http transport failed: {0}")]
    Transport(String),
}

/// A blocking HTTP client.
///
/// `fetch` runs on a dedicated thread, so implementations may block freely.
pub trait HttpClient: Send + Sync + 'static {
    /// Performs one request.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] only for transport failures; non-2xx responses
    /// are ordinary [`HttpResponseSpec`] values.
    fn fetch(&self, request: &HttpRequestSpec) -> Result<HttpResponseSpec, HttpError>;
}

/// A request queued by the host or another system.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    /// What to fetch.
    pub spec: HttpRequestSpec,
}

/// The result of one request, addressed by its id and record entity.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    /// The id assigned at dispatch time.
    pub id: u64,
    /// The entity holding the [`HttpRecord`].
    pub record: Entity,
    /// The outcome.
    pub result: Result<HttpResponseSpec, HttpError>,
}

/// The observable state of one request.
#[derive(Debug)]
pub struct HttpRecord {
    /// The id assigned at dispatch time.
    pub id: u64,
    /// The original request.
    pub spec: HttpRequestSpec,
    /// Pending until the response message arrives.
    pub state: HttpRecordState,
}

/// The state of an [`HttpRecord`].
#[derive(Debug)]
pub enum HttpRecordState {
    /// The request is still executing.
    Pending,
    /// The request finished; failures are transport errors, not status codes.
    Done(Result<HttpResponseSpec, HttpError>),
}

#[derive(Debug, Default)]
pub(crate) struct NextHttpId(u64);

pub(super) fn dispatch_http_requests(
    mut requests: MessageReader<HttpRequest>,
    client: Res<Arc<dyn HttpClient>>,
    mut next_id: ResMut<NextHttpId>,
    executor: Res<AsyncExecutor>,
    mut commands: Commands,
) {
    for request in requests.read() {
        let id = next_id.0;
        next_id.0 = next_id
            .0
            .checked_add(1)
            .expect("http request id space exhausted");

        let record = commands.spawn(HttpRecord {
            id,
            spec: request.spec.clone(),
            state: HttpRecordState::Pending,
        });

        let client = Arc::clone(&client);
        let spec = request.spec.clone();
        let executor = (*executor).clone();
        thread::spawn(move || {
            let result = client.fetch(&spec);
            if let Err(error) = executor.send(HttpResponse { id, record, result }) {
                log::warn!("failed to send http response: {error}");
            }
        });
    }
}

pub(super) fn handle_http_responses(
    mut responses: MessageReader<HttpResponse>,
    mut records: Query<&mut HttpRecord>,
) {
    for response in responses.read() {
        let Some(mut record) = records.get_mut(response.record) else {
            log::debug!(
                "dropping http response for missing record {:?}",
                response.record
            );
            continue;
        };
        if record.id != response.id {
            log::debug!(
                "dropping stale http response for record {:?}",
                response.record
            );
            continue;
        }
        record.state = HttpRecordState::Done(response.result.clone());
    }
}

/// Registers the demo HTTP resources and systems.
pub struct HttpPlugin {
    client: Arc<dyn HttpClient>,
}

impl HttpPlugin {
    /// Creates the plugin for a blocking client.
    pub fn new(client: Arc<dyn HttpClient>) -> Self {
        Self { client }
    }
}

impl Plugin for HttpPlugin {
    fn name(&self) -> &'static str {
        "demo-http"
    }

    fn build(&self, app: &mut App) {
        app.insert_resource(self.client.clone())
            .init_resource::<NextHttpId>()
            .add_systems(Update, dispatch_http_requests)
            .add_systems(Update, handle_http_responses);
    }
}

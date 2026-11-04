//! HTTP request example (`--example http_request`).
//!
//! The plugin lives in `demo` with zero extra dependencies; this example only
//! supplies the blocking client, backed by `ureq` from `dev-dependencies`.
//! Requests execute on their own threads, so no async runtime or task spawner
//! is needed — the executor here merely drives the runner future.

use std::sync::Arc;

use agnes_ecs::demo::{
    HttpClient, HttpError, HttpPlugin, HttpRecord, HttpRecordState, HttpRequest, HttpRequestSpec,
    HttpResponseSpec,
};
use agnes_ecs::{App, AsyncExecutor, Query, Res, ResMut, Update};

struct UreqClient;

impl HttpClient for UreqClient {
    fn fetch(&self, request: &HttpRequestSpec) -> Result<HttpResponseSpec, HttpError> {
        let request_line = ureq::request(request.method.as_str(), &request.url);
        let mut request_line = request_line;
        for (name, value) in &request.headers {
            request_line = request_line.set(name, value);
        }

        let outcome = match &request.body {
            Some(body) => request_line.send_string(body),
            None => request_line.call(),
        };

        match outcome {
            Ok(response) => {
                let status = response.status();
                let body = response
                    .into_string()
                    .map_err(|error| HttpError::Transport(error.to_string()))?;
                Ok(HttpResponseSpec { status, body })
            }
            // ureq treats non-2xx as an error; ehttp semantics keep the body.
            Err(ureq::Error::Status(_, response)) => {
                let status = response.status();
                Ok(HttpResponseSpec {
                    status,
                    body: response.into_string().unwrap_or_default(),
                })
            }
            Err(error) => Err(HttpError::Transport(error.to_string())),
        }
    }
}

#[derive(Default)]
struct SeedSent(bool);

fn request_seed(executor: Res<AsyncExecutor>, mut sent: ResMut<SeedSent>) {
    if sent.0 {
        return;
    }
    sent.0 = true;
    if let Err(error) = executor.send(HttpRequest {
        spec: HttpRequestSpec::get("https://api.ipify.org?format=json"),
    }) {
        log::warn!("failed to seed http request: {error}");
    }
}

fn report_records(query: Query<&HttpRecord>, executor: Res<AsyncExecutor>) {
    for record in query.iter() {
        if let HttpRecordState::Done(result) = &record.state {
            match result {
                Ok(response) => println!(
                    "[{}] {} -> {}",
                    response.status, record.spec.url, response.body
                ),
                Err(error) => println!("{} -> {error}", record.spec.url),
            }
            executor.cancellation().cancel();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executor = Arc::new(async_executor::Executor::new());
    let mut app = App::new();
    app.add_plugin(HttpPlugin::new(Arc::new(UreqClient)))
        .init_resource::<SeedSent>()
        .add_systems(Update, request_seed)
        .add_systems(Update, report_records);

    futures_lite::future::block_on(executor.run(async { app.run_async().await }))?;
    println!("runtime stopped");
    Ok(())
}

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_executor::Executor;
use async_trait::async_trait;
use event_listener::Event;
use futures_lite::future::block_on;

use crate::app::App;
use crate::entity::Entity;
use crate::runner::{SpawnError, TaskSpawner};

use super::{
    AgentPlugin, ApprovalLog, CancelRun, ModelCompleted, ModelError, ModelPlugin, ModelProvider,
    PermissionPlugin, PermissionPolicy, Rule, Tool, ToolError, ToolPlugin, Turn, UserInput,
};

struct EchoModel;

#[async_trait]
impl ModelProvider for EchoModel {
    fn name(&self) -> &str {
        "default"
    }

    async fn complete(&self, prompt: &str) -> Result<String, ModelError> {
        Ok(format!("echo: {prompt}"))
    }
}

struct ToolRequestingModel {
    tool: String,
}

#[async_trait]
impl ModelProvider for ToolRequestingModel {
    fn name(&self) -> &str {
        "default"
    }

    async fn complete(&self, _prompt: &str) -> Result<String, ModelError> {
        Ok(format!("tool:{}:payload", self.tool))
    }
}

struct CountingTool {
    name: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, arguments: &str) -> Result<String, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("ran {arguments}"))
    }
}

/// A two-party rendezvous: `wait` only resolves once two tasks reached it.
///
/// This replaces `tokio::sync::Barrier` and proves that both model calls are
/// concurrently in flight instead of polled one after another.
struct Rendezvous {
    arrived: AtomicUsize,
    release: Event,
}

impl Rendezvous {
    fn new() -> Self {
        Self {
            arrived: AtomicUsize::new(0),
            release: Event::new(),
        }
    }

    async fn wait(&self) {
        if self.arrived.fetch_add(1, Ordering::SeqCst) == 0 {
            let listener = self.release.listen();
            if self.arrived.load(Ordering::SeqCst) < 2 {
                listener.await;
            }
        } else {
            self.release.notify(usize::MAX);
        }
    }
}

struct BarrierModel {
    rendezvous: Arc<Rendezvous>,
}

#[async_trait]
impl ModelProvider for BarrierModel {
    fn name(&self) -> &str {
        "default"
    }

    async fn complete(&self, _prompt: &str) -> Result<String, ModelError> {
        self.rendezvous.wait().await;
        Ok("done".to_owned())
    }
}

struct PendingModel;

#[async_trait]
impl ModelProvider for PendingModel {
    fn name(&self) -> &str {
        "default"
    }

    async fn complete(&self, _prompt: &str) -> Result<String, ModelError> {
        std::future::pending::<Result<String, ModelError>>().await
    }
}

#[derive(Clone)]
struct TestSpawner {
    executor: Arc<Executor<'static>>,
}

impl TaskSpawner for TestSpawner {
    fn spawn(&self, task: Pin<Box<dyn Future<Output = ()> + Send>>) -> Result<(), SpawnError> {
        // `async_task::Task` cancels on drop; detach for fire-and-forget.
        self.executor.spawn(task).detach();
        Ok(())
    }
}

fn app_with(
    model: Arc<dyn ModelProvider>,
    tools: Vec<Arc<dyn Tool>>,
    policy: PermissionPolicy,
) -> (App, Arc<Executor<'static>>) {
    let executor = Arc::new(Executor::new());
    let mut app = App::new();
    app.add_plugin(AgentPlugin);
    app.add_plugin(ModelPlugin::new(model));

    let mut tool_plugin = ToolPlugin::empty();
    for tool in tools {
        tool_plugin = tool_plugin.with_tool(tool);
    }
    app.add_plugin(tool_plugin);
    app.add_plugin(PermissionPlugin::new(policy));
    app.set_spawner(TestSpawner {
        executor: executor.clone(),
    });
    (app, executor)
}

fn send_input(app: &App, text: &str) {
    app.runtime_sender()
        .send_message(UserInput {
            text: text.to_owned(),
        })
        .expect("runtime is alive");
}

fn turns(app: &App) -> Vec<(Entity, String, bool)> {
    let world = app.world();
    let mut turns = Vec::new();
    for entity in world.alive_entities() {
        if let Some(turn) = world.get::<Turn>(entity) {
            turns.push((entity, turn.last_text().to_owned(), turn.finished()));
        }
    }
    turns
}

fn only_turn(app: &App) -> (Entity, String, bool) {
    let turns = turns(app);
    assert_eq!(turns.len(), 1, "expected exactly one turn");
    turns.into_iter().next().expect("one turn")
}

async fn settle(app: &mut App) {
    for _ in 0..16 {
        futures_lite::future::yield_now().await;
        app.pump().expect("pump succeeds");
    }
}

#[test]
fn user_input_starts_run_and_model_response_finishes_turn() {
    let (mut app, executor) =
        app_with(Arc::new(EchoModel), Vec::new(), PermissionPolicy::default());
    block_on(executor.run(async {
        send_input(&app, "hi");
        app.pump().expect("start the run");
        settle(&mut app).await;

        let (_, text, finished) = only_turn(&app);
        assert_eq!(text, "echo: hi");
        assert!(finished);
    }));
}

#[test]
fn registries_are_plugin_extensible() {
    let (app, _executor) = app_with(
        Arc::new(EchoModel),
        vec![Arc::new(CountingTool {
            name: "echo".to_owned(),
            calls: Arc::new(AtomicUsize::new(0)),
        })],
        PermissionPolicy::default(),
    );

    let world = app.world();
    let models = world
        .resource::<super::ModelRegistry>()
        .expect("model registry");
    assert_eq!(models.names().collect::<Vec<_>>(), ["default"]);

    let tools = world
        .resource::<super::ToolRegistry>()
        .expect("tool registry");
    assert_eq!(tools.names().collect::<Vec<_>>(), ["echo"]);
}

#[test]
fn permission_deny_prevents_tool_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(CountingTool {
        name: "hidden".to_owned(),
        calls: Arc::clone(&calls),
    });
    let (mut app, executor) = app_with(
        Arc::new(ToolRequestingModel {
            tool: "hidden".to_owned(),
        }),
        vec![tool],
        PermissionPolicy {
            deny: vec![Rule::name("hidden")],
            ask: Vec::new(),
        },
    );

    block_on(executor.run(async {
        send_input(&app, "go");
        app.pump().expect("start the run");
        settle(&mut app).await;

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let (_, text, finished) = only_turn(&app);
        assert!(text.contains("denied"));
        assert!(finished);
    }));
}

#[test]
fn permission_ask_emits_an_approval_request() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(CountingTool {
        name: "echo".to_owned(),
        calls: Arc::clone(&calls),
    });
    let (mut app, executor) = app_with(
        Arc::new(ToolRequestingModel {
            tool: "echo".to_owned(),
        }),
        vec![tool],
        PermissionPolicy {
            deny: Vec::new(),
            ask: vec![Rule::name("echo")],
        },
    );

    block_on(executor.run(async {
        send_input(&app, "go");
        app.pump().expect("start the run");
        settle(&mut app).await;

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let log = app.world().resource::<ApprovalLog>().expect("approval log");
        assert_eq!(log.requests.len(), 1);
        assert_eq!(log.requests[0].name, "echo");
    }));
}

#[test]
fn an_approved_tool_runs_and_finishes_the_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(CountingTool {
        name: "echo".to_owned(),
        calls: Arc::clone(&calls),
    });
    let (mut app, executor) = app_with(
        Arc::new(ToolRequestingModel {
            tool: "echo".to_owned(),
        }),
        vec![tool],
        PermissionPolicy {
            deny: Vec::new(),
            ask: vec![Rule::name("echo")],
        },
    );

    block_on(executor.run(async {
        send_input(&app, "go");
        app.pump().expect("start the run");
        settle(&mut app).await;

        let request = {
            let log = app.world().resource::<ApprovalLog>().expect("approval log");
            log.requests[0].clone()
        };
        app.runtime_sender()
            .send_message(super::ApprovalResolved {
                run: request.run,
                version: request.version,
                name: request.name,
                arguments: request.arguments,
                approved: true,
            })
            .expect("runtime is alive");
        settle(&mut app).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let (_, text, finished) = only_turn(&app);
        assert_eq!(text, "ran payload");
        assert!(finished);
    }));
}

#[test]
fn two_runs_wait_for_models_concurrently() {
    let (mut app, executor) = app_with(
        Arc::new(BarrierModel {
            rendezvous: Arc::new(Rendezvous::new()),
        }),
        Vec::new(),
        PermissionPolicy::default(),
    );
    block_on(executor.run(async {
        send_input(&app, "first");
        send_input(&app, "second");
        app.pump().expect("start both runs");

        let mut both_finished = false;
        for _ in 0..1000 {
            futures_lite::future::yield_now().await;
            app.pump().expect("pump succeeds");
            if turns(&app)
                .iter()
                .filter(|(_, _, finished)| *finished)
                .count()
                == 2
            {
                both_finished = true;
                break;
            }
        }
        assert!(both_finished, "both model calls reached the rendezvous");
    }));
}

#[test]
fn a_late_model_result_after_cancel_is_ignored() {
    let (mut app, executor) = app_with(
        Arc::new(PendingModel),
        Vec::new(),
        PermissionPolicy::default(),
    );
    block_on(executor.run(async {
        send_input(&app, "go");
        app.pump().expect("start the run");

        let (run, before, _) = only_turn(&app);
        app.runtime_sender()
            .send_message(CancelRun { run })
            .expect("runtime is alive");
        app.pump().expect("cancel the run");

        app.runtime_sender()
            .send_message(ModelCompleted {
                run,
                version: 0,
                text: "late result".to_owned(),
            })
            .expect("runtime is alive");
        settle(&mut app).await;

        let (_, text, finished) = only_turn(&app);
        assert_eq!(text, before);
        assert!(finished);
    }));
}

#[test]
fn a_late_model_result_after_despawn_is_ignored() {
    let (mut app, executor) = app_with(
        Arc::new(PendingModel),
        Vec::new(),
        PermissionPolicy::default(),
    );
    block_on(executor.run(async {
        send_input(&app, "go");
        app.pump().expect("start the run");

        let (run, _, _) = only_turn(&app);
        assert!(app.world_mut().despawn(run));

        app.runtime_sender()
            .send_message(ModelCompleted {
                run,
                version: 0,
                text: "late result".to_owned(),
            })
            .expect("runtime is alive");
        settle(&mut app).await;

        assert!(turns(&app).is_empty());
    }));
}

// ---- HTTP ----

use super::{
    HttpClient, HttpError, HttpPlugin, HttpRecord, HttpRecordState, HttpRequest, HttpRequestSpec,
    HttpResponseSpec,
};

/// A client that blocks until the test releases it, making thread completion
/// deterministic instead of racing the pump loop.
struct FakeClient {
    gate: async_channel::Receiver<()>,
    result: Result<HttpResponseSpec, HttpError>,
}

impl HttpClient for FakeClient {
    fn fetch(&self, _request: &HttpRequestSpec) -> Result<HttpResponseSpec, HttpError> {
        let _ = self.gate.recv_blocking();
        self.result.clone()
    }
}

struct UrlEchoClient {
    gate: async_channel::Receiver<()>,
}

impl HttpClient for UrlEchoClient {
    fn fetch(&self, request: &HttpRequestSpec) -> Result<HttpResponseSpec, HttpError> {
        let _ = self.gate.recv_blocking();
        Ok(HttpResponseSpec {
            status: 200,
            body: format!("resp:{}", request.url),
        })
    }
}

fn http_app(client: Arc<dyn HttpClient>) -> App {
    let mut app = App::new();
    app.add_plugin(HttpPlugin::new(client));
    app
}

/// Returns `(id, body)` per record; `body` is `None` while still pending.
fn http_record_bodies(app: &App) -> Vec<(u64, Option<String>)> {
    let world = app.world();
    let mut records = Vec::new();
    for entity in world.alive_entities() {
        if let Some(record) = world.get::<HttpRecord>(entity) {
            let body = match &record.state {
                HttpRecordState::Pending => None,
                HttpRecordState::Done(Ok(response)) => Some(response.body.clone()),
                HttpRecordState::Done(Err(error)) => Some(format!("error: {error}")),
            };
            records.push((record.id, body));
        }
    }
    records
}

fn wait_until_all_records_settle(app: &mut App, expected: usize) {
    for _ in 0..1000 {
        app.pump().expect("pump succeeds");
        let bodies = http_record_bodies(app);
        if bodies.len() == expected && bodies.iter().all(|(_, body)| body.is_some()) {
            return;
        }
        // Give the request threads a chance to run.
        std::thread::yield_now();
    }
    panic!("http records did not settle: {:?}", http_record_bodies(app));
}

#[test]
fn an_http_request_updates_its_record_with_the_response_body() {
    let (gate_tx, gate_rx) = async_channel::unbounded();
    let mut app = http_app(Arc::new(FakeClient {
        gate: gate_rx,
        result: Ok(HttpResponseSpec {
            status: 200,
            body: "hello".to_owned(),
        }),
    }));

    app.runtime_sender()
        .send_message(HttpRequest {
            spec: HttpRequestSpec::get("http://example.test/hello"),
        })
        .expect("runtime is alive");
    app.pump().expect("dispatch the request");
    gate_tx.try_send(()).expect("gate is open");
    wait_until_all_records_settle(&mut app, 1);

    assert_eq!(http_record_bodies(&app), [(0, Some("hello".to_owned()))]);
}

#[test]
fn an_http_transport_failure_is_recorded_not_fatal() {
    let (gate_tx, gate_rx) = async_channel::unbounded();
    let mut app = http_app(Arc::new(FakeClient {
        gate: gate_rx,
        result: Err(HttpError::Transport("boom".to_owned())),
    }));

    app.runtime_sender()
        .send_message(HttpRequest {
            spec: HttpRequestSpec::get("http://example.test/fail"),
        })
        .expect("runtime is alive");
    app.pump().expect("dispatch the request");
    gate_tx.try_send(()).expect("gate is open");
    wait_until_all_records_settle(&mut app, 1);

    let (_, body) = &http_record_bodies(&app)[0];
    let body = body.as_deref().expect("record settled");
    assert!(body.contains("boom"), "unexpected body: {body}");
}

#[test]
fn concurrent_http_responses_stay_paired_with_their_requests() {
    let (gate_tx, gate_rx) = async_channel::unbounded();
    let mut app = http_app(Arc::new(UrlEchoClient { gate: gate_rx }));

    for url in ["http://example.test/a", "http://example.test/b"] {
        app.runtime_sender()
            .send_message(HttpRequest {
                spec: HttpRequestSpec::get(url),
            })
            .expect("runtime is alive");
    }
    app.pump().expect("dispatch both requests");
    for _ in 0..2 {
        gate_tx.try_send(()).expect("gate is open");
    }
    wait_until_all_records_settle(&mut app, 2);

    let mut bodies: Vec<String> = http_record_bodies(&app)
        .into_iter()
        .map(|(_, body)| body.expect("record settled"))
        .collect();
    bodies.sort();
    assert_eq!(
        bodies,
        [
            "resp:http://example.test/a".to_owned(),
            "resp:http://example.test/b".to_owned()
        ]
    );
}

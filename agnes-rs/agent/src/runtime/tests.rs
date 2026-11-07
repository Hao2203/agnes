//! 消息管线测试：脚本化的 provider 与工具驱动完整 turn，覆盖正常往返、
//! 工具轮、门禁、审批、打断、取消与失败收场。

use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agnes_core::config::{ModelName, ReasoningEffort, StepBudget, SystemPrompt, Thinking};
use agnes_core::content::Content;
use agnes_core::header::{Environment, Header};
use agnes_core::llm::{LlmResponse, StopReason};
use agnes_core::message::{TokenUsage, ToolArguments, ToolCall, ToolCallId, ToolOutput};
use agnes_core::tool::{JsonSchema, ToolError, ToolName, ToolSpec};
use agnes_core::turn::{AgentConfig, Outcome};
use agnes_ecs::{
    App, Commands, Entity, MessageReader, MessageWriter, Query, Res, ResMut, RuntimeSender, Update,
};
use agnes_io::http::TransportError;
use agnes_io::llm::{LlmCall, LlmCancel, LlmError, LlmProvider, LlmReply};
use agnes_io::tool::Tool;
use agnes_plugin_async::TokioPlugin;
use agnes_plugin_session::SessionPlugin;
use agnes_record::{
    Durability, Event, HostEvent, RotateSession, RunEvent, RunEventKind, SessionError,
    SessionStore, SessionStoreResource,
};
use agnes_util::NonEmpty;
use async_trait::async_trait;
use jiff::Timestamp;

use super::messages::{
    ApprovalDecided, CancelTurn, InterruptTurn, TurnFailure, TurnFailureKind, TurnRequested,
    UserMessage,
};
use super::request::{TurnRequest, TurnRequestId, TurnRequestSeq, TurnResponder, TurnResponse};
use super::{ActiveTurn, AgentPlugin, RequestBuilderResource, ToolsPlugin};
use crate::AgentCore;
use crate::StandardRequestBuilder;
use crate::approval::ApprovalChannel;
use crate::host::AgentBuilder;
use crate::model::{ModelPlugin, SetModel, SetThinking};
use crate::permission::{PermissionRules, Rule};

const MODEL: &str = "test-model";

/// The host-spawned agent.
fn agent(app: &App) -> Entity {
    app.world()
        .entity_with::<crate::AgentCore>()
        .expect("agent exists")
}

fn capture(app: &App) -> Capture {
    app.world()
        .resource::<Capture>()
        .expect("capture is installed")
        .clone()
}

#[derive(Default, Clone)]
struct Capture {
    finished: Vec<Result<Outcome, TurnFailure>>,
    ids: Vec<TurnRequestId>,
    goals: Vec<Option<String>>,
    run_events: Vec<RunEvent>,
}

/// 拉取式宿主：读取响应组件后 despawn 请求实体，恰好一次。
fn capture_finished(
    mut commands: Commands,
    responses: Query<(Entity, &TurnRequest, &TurnResponse)>,
    mut capture: ResMut<Capture>,
) {
    for (entity, request, response) in responses.iter() {
        capture.finished.push(match &response.result {
            Ok(result) => Ok(result.outcome),
            Err(failure) => Err(failure.clone()),
        });
        capture.ids.push(response.id);
        capture.goals.push(
            request
                .config
                .goal
                .as_ref()
                .map(|goal| goal.as_str().to_owned()),
        );
        commands.entity(entity).despawn();
    }
}

fn capture_run_events(mut reader: MessageReader<RunEvent>, mut capture: ResMut<Capture>) {
    capture
        .run_events
        .extend(reader.read().iter().map(|event| (**event).clone()));
}

fn text_response(content: &str) -> LlmResponse {
    LlmResponse {
        stop_reason: StopReason::Completed {
            content: Content::try_from(content).expect("static content is valid"),
        },
        usage: TokenUsage {
            input: 1,
            output: 1,
        },
    }
}

fn tool_response() -> LlmResponse {
    LlmResponse {
        stop_reason: StopReason::ToolUse {
            content: None,
            calls: NonEmpty::new(ToolCall {
                id: ToolCallId::new("c1"),
                name: ToolName::from_str("echo_tool").expect("static name is valid"),
                arguments: ToolArguments::new("{}"),
            }),
        },
        usage: TokenUsage {
            input: 1,
            output: 1,
        },
    }
}

/// 脚本化的 provider：读 `LlmCall`，弹一条预设回复写回 `LlmReply`；
/// 队列空了就回一条普通的最终答复。`muted` 时永不回复（打断测试）。
#[derive(Default)]
struct ProviderState {
    replies: Mutex<VecDeque<Result<LlmResponse, LlmError>>>,
    calls: AtomicUsize,
    cancels: AtomicUsize,
    last: Arc<Mutex<Option<(String, Thinking)>>>,
    muted: bool,
}

impl ProviderState {
    fn with_replies(replies: Vec<Result<LlmResponse, LlmError>>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            ..Self::default()
        }
    }

    fn muted() -> Self {
        Self {
            muted: true,
            ..Self::default()
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn cancels(&self) -> usize {
        self.cancels.load(Ordering::SeqCst)
    }

    fn last_handle(&self) -> Arc<Mutex<Option<(String, Thinking)>>> {
        Arc::clone(&self.last)
    }
}

fn answer_calls(
    provider: Res<ProviderState>,
    mut calls: MessageReader<LlmCall>,
    mut replies: MessageWriter<LlmReply>,
) {
    for call in calls.read() {
        provider.calls.fetch_add(1, Ordering::SeqCst);
        *provider.last.lock().expect("last lock") = Some((
            call.request.model.as_str().to_owned(),
            call.request.thinking,
        ));
        if provider.muted {
            continue;
        }
        let result = provider
            .replies
            .lock()
            .expect("replies lock")
            .pop_front()
            .unwrap_or_else(|| Ok(text_response("echo")));
        replies.write(LlmReply {
            id: call.id,
            result,
        });
    }
}

fn count_cancels(provider: Res<ProviderState>, mut cancels: MessageReader<LlmCancel>) {
    provider
        .cancels
        .fetch_add(cancels.read().len(), Ordering::SeqCst);
}

struct EchoTool {
    spec: ToolSpec,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                ToolName::from_str("echo_tool").expect("static name is valid"),
                "echoes the arguments",
                JsonSchema::try_from(serde_json::json!({"type": "object"}))
                    .expect("static schema is valid"),
            ),
        }
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success("tool ran"))
    }
}

/// 睡眠远超测试时长的工具，用于打断路径。
struct SlowTool {
    spec: ToolSpec,
}

impl SlowTool {
    fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                ToolName::from_str("echo_tool").expect("static name is valid"),
                "sleeps",
                JsonSchema::try_from(serde_json::json!({"type": "object"}))
                    .expect("static schema is valid"),
            ),
        }
    }
}

#[async_trait]
impl Tool for SlowTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        Ok(ToolOutput::success("tool ran"))
    }
}

fn header() -> Header {
    Header::new(SystemPrompt::new("test"), Environment::default())
}

fn config(goal: &str) -> AgentConfig {
    AgentConfig {
        header: header(),
        goal: Some(Content::try_from(goal).expect("static goal is valid")),
        budget: StepBudget::new(NonZeroU32::new(8).expect("non-zero")),
    }
}

fn model(name: &str) -> ModelName {
    ModelName::from_str(name).expect("static model name is valid")
}

/// 宿主装配：一次挂齐核心状态、输入队列与会话能力。
fn spawn_agent(app: &mut App) -> Entity {
    AgentBuilder::new(model(MODEL), Thinking::Off)
        .spawn(app)
        .expect("agent spawns")
        .entity
}

fn seq_of(app: &App) -> TurnRequestSeq {
    app.world()
        .resource::<TurnRequestSeq>()
        .expect("AgentPlugin or AgentBuilder installs the seq")
        .clone()
}

/// 从 `World` 直接提交（同步测试用）。
fn send_request(app: &mut App, agent: Entity, goal: &str) -> TurnRequestId {
    let id = seq_of(app).next();
    app.world_mut().send_message(TurnRequested {
        id,
        agent,
        config: config(goal),
        responder: None,
    });
    id
}

fn submit(sender: &RuntimeSender, seq: &TurnRequestSeq, agent: Entity, goal: &str) {
    sender
        .send_message(TurnRequested {
            id: seq.next(),
            agent,
            config: config(goal),
            responder: None,
        })
        .expect("runtime is alive");
}

fn app(provider: ProviderState) -> App {
    app_with(provider, ToolsPlugin::empty())
}

/// 只装插件、不 spawn agent 实体，也不注册任何响应消费者。
fn bare_app_without_capture() -> App {
    let mut app = App::new();
    app.add_plugin(SessionPlugin)
        .add_plugin(ModelPlugin)
        .add_plugin(ToolsPlugin::empty())
        .add_plugin(AgentPlugin)
        .insert_resource(LlmProvider)
        .insert_resource(RequestBuilderResource::new(Box::new(
            StandardRequestBuilder,
        )))
        .init_resource::<TurnRequestSeq>()
        .add_plugin(TokioPlugin);
    app
}

/// 只装插件、不 spawn agent 实体：用于装配校验测试。
fn bare_app() -> App {
    let mut app = bare_app_without_capture();
    app.insert_resource(Capture::default())
        .add_systems(Update, capture_finished);
    app
}

fn app_with(provider: ProviderState, tools: ToolsPlugin) -> App {
    let mut app = App::new();
    spawn_agent(&mut app);
    app.add_plugin(SessionPlugin)
        .add_plugin(ModelPlugin)
        .add_plugin(tools)
        .add_plugin(AgentPlugin)
        .insert_resource(provider)
        .insert_resource(LlmProvider)
        .insert_resource(RequestBuilderResource::new(Box::new(
            StandardRequestBuilder,
        )))
        .insert_resource(Capture::default())
        .add_systems(Update, capture_finished)
        .add_systems(Update, capture_run_events)
        .add_systems(Update, answer_calls)
        .add_systems(Update, count_cancels)
        .add_plugin(TokioPlugin);
    app
}

fn provider_calls(app: &App) -> usize {
    app.world()
        .resource::<ProviderState>()
        .expect("provider is installed")
        .calls()
}

fn provider_cancels(app: &App) -> usize {
    app.world()
        .resource::<ProviderState>()
        .expect("provider is installed")
        .cancels()
}

async fn drive(mut app: App, seed: impl FnOnce(&RuntimeSender)) -> Capture {
    seed(&app.runtime_sender());
    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if !capture(&app).finished.is_empty() {
            return capture(&app);
        }
        tokio::task::yield_now().await;
    }
    panic!("the turn did not finish");
}

#[tokio::test]
async fn a_turn_completes_with_a_final_answer() {
    let app = app(ProviderState::default());
    let agent = agent(&app);
    let seq = seq_of(&app);
    let capture = drive(app, |sender| submit(sender, &seq, agent, "hello")).await;

    assert_eq!(capture.finished, vec![Ok(Outcome::Answered)]);
    assert!(matches!(
        capture.run_events.first().map(|event| &event.kind),
        Some(RunEventKind::Started { .. })
    ));
    assert!(matches!(
        capture.run_events.last().map(|event| &event.kind),
        Some(RunEventKind::Finished {
            outcome: Outcome::Answered,
            ..
        })
    ));
}

#[tokio::test]
async fn a_model_switch_changes_the_next_request() {
    let provider = ProviderState::default();
    let last = provider.last_handle();
    let mut app = app(provider);
    let agent = agent(&app);
    let seq = seq_of(&app);
    app.world_mut().send_message(SetModel {
        agent,
        model: model("other-model"),
    });
    app.world_mut().send_message(SetThinking {
        agent,
        thinking: Thinking::On(ReasoningEffort::Low),
    });
    app.pump().expect("settings apply");

    let capture = drive(app, |sender| submit(sender, &seq, agent, "hello")).await;

    assert_eq!(capture.finished, vec![Ok(Outcome::Answered)]);
    let (model, thinking) = last
        .lock()
        .expect("last lock")
        .clone()
        .expect("a request was sent");
    assert_eq!(model, "other-model");
    assert_eq!(thinking, Thinking::On(ReasoningEffort::Low));
}

#[derive(Default)]
struct ActiveFlag(bool);

fn track_active(turns: Query<&ActiveTurn>, mut flag: ResMut<ActiveFlag>) {
    flag.0 = !turns.is_empty();
}

fn active_turn(app: &App) -> bool {
    app.world()
        .resource::<ActiveFlag>()
        .expect("active flag is installed")
        .0
}

#[tokio::test]
async fn the_active_turn_marker_tracks_the_turn_lifecycle() {
    let mut app = app(ProviderState::muted());
    app.insert_resource(ActiveFlag::default());
    app.add_systems(Update, track_active);
    let agent = agent(&app);
    let sender = app.runtime_sender();
    sender
        .send_message(TurnRequested {
            id: seq_of(&app).next(),
            agent,
            config: config("hold"),
            responder: None,
        })
        .expect("runtime is alive");

    app.pump().expect("schedule runs");
    assert!(
        active_turn(&app),
        "the marker appears while the turn is in flight"
    );

    sender
        .send_message(InterruptTurn { agent })
        .expect("runtime is alive");
    app.pump().expect("schedule runs");
    assert!(
        !active_turn(&app),
        "the marker disappears when the turn closes"
    );
}

#[tokio::test]
async fn queued_turn_requests_run_one_after_another() {
    let app = app(ProviderState::default());
    let agent = agent(&app);
    let seq = seq_of(&app);
    let mut app = app;
    let sender = app.runtime_sender();
    submit(&sender, &seq, agent, "first");
    submit(&sender, &seq, agent, "second");
    for _ in 0..512 {
        app.pump().expect("schedule runs");
        if capture(&app).finished.len() == 2 {
            let capture = capture(&app);
            assert_eq!(
                capture.finished,
                vec![Ok(Outcome::Answered), Ok(Outcome::Answered)]
            );
            assert_eq!(capture.ids.len(), 2, "each response keeps its request id");
            assert_ne!(capture.ids[0], capture.ids[1]);
            assert_eq!(
                capture.goals,
                vec![Some("first".to_owned()), Some("second".to_owned())],
                "each response maps back to its own request"
            );
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("both turns did not finish");
}

#[derive(Default)]
struct ActiveCount(usize);

fn track_active_count(turns: Query<&ActiveTurn>, mut count: ResMut<ActiveCount>) {
    count.0 = turns.iter().count();
}

#[tokio::test]
async fn each_agent_runs_its_own_turn_concurrently() {
    let mut app = app(ProviderState::muted());
    app.insert_resource(ActiveCount::default());
    app.add_systems(Update, track_active_count);

    // 第二个 agent 实体：宿主按同样的装配流程 spawn。
    let first = agent(&app);
    let second = spawn_agent(&mut app);
    let seq = seq_of(&app);

    let sender = app.runtime_sender();
    for agent in [first, second] {
        submit(&sender, &seq, agent, "hold");
    }
    app.pump().expect("schedule runs");

    let count = app
        .world()
        .resource::<ActiveCount>()
        .expect("count is installed")
        .0;
    assert_eq!(
        count, 2,
        "each agent runs its own turn; neither queues behind the other"
    );
}

#[tokio::test]
async fn a_tool_round_runs_the_tool_and_completes() {
    let app = app_with(
        ProviderState::with_replies(vec![Ok(tool_response())]),
        ToolsPlugin::new(Arc::new(EchoTool::new())),
    );
    let agent = agent(&app);
    let seq = seq_of(&app);
    let capture = drive(app, |sender| submit(sender, &seq, agent, "use the tool")).await;

    assert_eq!(capture.finished, vec![Ok(Outcome::Answered)]);
    let tool_events = capture
        .run_events
        .iter()
        .filter(|event| {
            matches!(
                &event.kind,
                RunEventKind::Recorded {
                    event: agnes_record::Event::ToolExecutionStart { .. },
                    ..
                } | RunEventKind::Recorded {
                    event: agnes_record::Event::ToolExecutionEnd { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(tool_events, 2, "one start and one end event");
}

#[tokio::test]
async fn a_denied_tool_settles_with_a_synthetic_failure() {
    let mut app = app_with(
        ProviderState::with_replies(vec![Ok(tool_response())]),
        ToolsPlugin::new(Arc::new(EchoTool::new())),
    );
    app.insert_resource(PermissionRules {
        deny: vec![Rule::Prefix("echo".into())],
        ask: vec![],
    });
    let agent = agent(&app);
    let seq = seq_of(&app);
    let capture = drive(app, |sender| submit(sender, &seq, agent, "use the tool")).await;

    assert_eq!(capture.finished, vec![Ok(Outcome::Answered)]);
    assert!(
        capture
            .run_events
            .iter()
            .any(|event| matches!(&event.kind, RunEventKind::ToolGated { .. }))
    );
}

#[tokio::test]
async fn an_asked_tool_runs_after_the_host_answers() {
    let mut app = app_with(
        ProviderState::with_replies(vec![Ok(tool_response())]),
        ToolsPlugin::new(Arc::new(EchoTool::new())),
    );
    app.insert_resource(PermissionRules {
        deny: vec![],
        ask: vec![Rule::Name(
            ToolName::from_str("echo_tool").expect("static name is valid"),
        )],
    });
    let (channel, _receiver) = ApprovalChannel::channel(None);
    app.insert_resource(channel);

    let agent = agent(&app);
    let sender = app.runtime_sender();
    sender
        .send_message(TurnRequested {
            id: seq_of(&app).next(),
            agent,
            config: config("use the tool"),
            responder: None,
        })
        .expect("runtime is alive");

    let mut approved = false;
    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if !approved
            && capture(&app)
                .run_events
                .iter()
                .any(|event| matches!(&event.kind, RunEventKind::ApprovalRequested { .. }))
        {
            sender
                .send_message(ApprovalDecided {
                    agent,
                    call_id: ToolCallId::new("c1"),
                    approved: true,
                })
                .expect("runtime is alive");
            approved = true;
        }
        if capture(&app).finished.len() == 1 {
            let capture = capture(&app);
            assert_eq!(capture.finished, vec![Ok(Outcome::Answered)]);
            assert!(approved);
            assert!(capture.run_events.iter().any(|event| matches!(
                &event.kind,
                RunEventKind::ApprovalResolved {
                    outcome: agnes_record::ApprovalOutcome::AllowedOnce,
                    ..
                }
            )));
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the turn did not finish");
}

#[tokio::test]
async fn an_interrupt_closes_the_turn_with_a_not_run_result() {
    let app = app_with(
        ProviderState::with_replies(vec![Ok(tool_response())]),
        ToolsPlugin::new(Arc::new(SlowTool::new())),
    );
    let mut app = app;
    let agent = agent(&app);
    let sender = app.runtime_sender();
    sender
        .send_message(TurnRequested {
            id: seq_of(&app).next(),
            agent,
            config: config("use the tool"),
            responder: None,
        })
        .expect("runtime is alive");

    for _ in 0..256 {
        app.pump().expect("schedule runs");
        let tool_started = capture(&app).run_events.iter().any(|event| {
            matches!(
                &event.kind,
                RunEventKind::Recorded {
                    event: agnes_record::Event::ToolExecutionStart { .. },
                    ..
                }
            )
        });
        if tool_started {
            sender
                .send_message(InterruptTurn { agent })
                .expect("runtime is alive");
        }
        if capture(&app).finished.len() == 1 {
            let capture = capture(&app);
            assert_eq!(capture.finished, vec![Ok(Outcome::Interrupted)]);
            assert!(capture.run_events.iter().any(|event| matches!(
                &event.kind,
                RunEventKind::Recorded {
                    event: agnes_record::Event::StepEnd {
                        step: _,
                        outcome: agnes_record::StepOutcome::Interrupted,
                    },
                    ..
                }
            )));
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the turn did not finish");
}

#[tokio::test]
async fn an_interrupt_mid_call_cancels_the_model_request() {
    let mut app = app(ProviderState::muted());
    let agent = agent(&app);
    let sender = app.runtime_sender();
    sender
        .send_message(TurnRequested {
            id: seq_of(&app).next(),
            agent,
            config: config("hello"),
            responder: None,
        })
        .expect("runtime is alive");

    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if provider_calls(&app) == 1 {
            sender
                .send_message(InterruptTurn { agent })
                .expect("runtime is alive");
        }
        if capture(&app).finished.len() == 1 {
            assert_eq!(
                capture(&app).finished,
                vec![Ok(Outcome::Interrupted)],
                "the turn closes without waiting for the model"
            );
            assert_eq!(provider_cancels(&app), 1, "the in-flight call is cancelled");
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the turn did not finish");
}

#[tokio::test]
async fn a_failing_model_fails_the_turn_and_the_app_survives() {
    let mut app = app(ProviderState::with_replies(vec![
        Err(LlmError::Transport(TransportError::Timeout)),
        Err(LlmError::Transport(TransportError::Timeout)),
    ]));
    let agent = agent(&app);
    let seq = seq_of(&app);
    let sender = app.runtime_sender();
    for _ in 0..2 {
        submit(&sender, &seq, agent, "hello");
    }
    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if capture(&app).finished.len() == 2 {
            let capture = capture(&app);
            assert_eq!(capture.finished.len(), 2);
            for outcome in &capture.finished {
                let failure = outcome.as_ref().expect_err("model fails");
                assert_eq!(failure.kind, TurnFailureKind::Model);
                assert!(failure.cause.contains("timed out"));
            }
            assert!(
                capture
                    .run_events
                    .iter()
                    .any(|event| matches!(&event.kind, RunEventKind::Failed { .. }))
            );
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the turns did not finish");
}

#[tokio::test]
async fn a_session_failure_fails_the_turn_and_the_app_survives() {
    let mut app = app(ProviderState::default());
    let agent = agent(&app);
    app.world_mut().insert(
        agent,
        agnes_record::Durability {
            sent_seq: 1,
            flushed_seq: 1,
            failed: Some("disk full".into()),
        },
    );
    let sender = app.runtime_sender();
    sender
        .send_message(TurnRequested {
            id: seq_of(&app).next(),
            agent,
            config: config("hello"),
            responder: None,
        })
        .expect("runtime is alive");

    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if !capture(&app).finished.is_empty() {
            let snapshot = capture(&app);
            let Err(failure) = &snapshot.finished[0] else {
                panic!("the session failure must fail the turn");
            };
            assert_eq!(failure.kind, TurnFailureKind::Session);
            assert_eq!(failure.cause, "disk full");
            // 应用存活：下一个 turn 不再发请求，同样立即失败收场。
            sender
                .send_message(TurnRequested {
                    id: seq_of(&app).next(),
                    agent,
                    config: config("again"),
                    responder: None,
                })
                .expect("runtime is alive");
            for _ in 0..256 {
                app.pump().expect("schedule runs");
                if capture(&app).finished.len() == 2 {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("the second turn did not finish");
        }
        tokio::task::yield_now().await;
    }
    panic!("the first turn did not finish");
}

/// 永不确认 flush 的存储：落盘前耐久门禁永远等不到 ack。
struct StuckStore;

#[async_trait]
impl SessionStore for StuckStore {
    async fn append(&self, _at: Timestamp, _event: &Event) -> Result<(), SessionError> {
        Ok(())
    }

    async fn append_host(&self, _at: Timestamp, _event: &HostEvent) -> Result<(), SessionError> {
        Ok(())
    }

    async fn flush(&self) -> Result<(), SessionError> {
        std::future::pending().await
    }

    async fn rotate(&self) -> Result<std::path::PathBuf, SessionError> {
        Ok(std::path::PathBuf::from("never"))
    }
}

#[tokio::test]
async fn the_durability_gate_holds_the_request_back() {
    let mut app = app(ProviderState::default());
    let agent = agent(&app);
    // 存储在但 flush 永不返回：sent_seq 随日志前进，flushed_seq 停在原地。
    let stuck: Arc<dyn SessionStore> = Arc::new(StuckStore);
    app.world_mut()
        .insert(agent, SessionStoreResource::new(stuck));
    // 预置一笔未确认的账：门禁从第一个 tick 起就是关的。
    app.world_mut().insert(
        agent,
        Durability {
            sent_seq: 5,
            flushed_seq: 0,
            failed: None,
        },
    );
    let sender = app.runtime_sender();
    sender
        .send_message(TurnRequested {
            id: seq_of(&app).next(),
            agent,
            config: config("hello"),
            responder: None,
        })
        .expect("runtime is alive");

    for _ in 0..64 {
        app.pump().expect("schedule runs");
        tokio::task::yield_now().await;
    }
    // 没有落盘确认前，模型调用不发生，turn 永远停在待调用阶段。
    assert!(capture(&app).finished.is_empty());
    assert_eq!(provider_calls(&app), 0, "no call is issued");
}

#[test]
#[should_panic(expected = "AgentPlugin requires SessionPlugin")]
fn agent_plugin_requires_session_plugin_first() {
    let mut app = App::new();
    spawn_agent(&mut app);
    app.add_plugin(ModelPlugin)
        .add_plugin(ToolsPlugin::empty())
        .add_plugin(AgentPlugin);
}

#[test]
#[should_panic(expected = "AgentPlugin requires ModelPlugin")]
fn agent_plugin_requires_model_plugin_first() {
    let mut app = App::new();
    spawn_agent(&mut app);
    app.add_plugin(SessionPlugin)
        .add_plugin(ToolsPlugin::empty())
        .add_plugin(AgentPlugin);
}

#[test]
#[should_panic(expected = "AgentPlugin requires ToolsPlugin")]
fn agent_plugin_requires_tools_plugin_first() {
    let mut app = App::new();
    spawn_agent(&mut app);
    app.add_plugin(SessionPlugin)
        .add_plugin(ModelPlugin)
        .add_plugin(AgentPlugin);
}

#[test]
fn agent_plugin_registers_without_an_agent() {
    let _app = bare_app();
}

#[test]
fn a_turn_request_to_a_non_agent_entity_finishes_with_an_error() {
    let mut app = bare_app();
    let not_an_agent = app.world_mut().spawn_empty();
    send_request(&mut app, not_an_agent, "hello");
    app.pump().expect("the schedule runs without panicking");

    let capture = capture(&app);
    assert_eq!(capture.finished.len(), 1);
    let Err(failure) = &capture.finished[0] else {
        panic!("the turn must fail: {:?}", capture.finished);
    };
    assert_eq!(
        failure.kind,
        TurnFailureKind::MissingCore,
        "the failure is typed, not a message-string match"
    );
}

#[test]
fn a_response_is_a_component_not_a_message() {
    let mut app = bare_app_without_capture();
    let not_an_agent = app.world_mut().spawn_empty();
    let id = send_request(&mut app, not_an_agent, "hello");
    // 没有注册任何响应消费者：响应仍然落在请求实体上。
    app.pump().expect("the schedule runs");
    let entity = app
        .world()
        .entity_with::<TurnResponse>()
        .expect("a response component exists");
    let response = app
        .world()
        .get::<TurnResponse>(entity)
        .expect("response component");
    assert_eq!(response.id, id);
    assert!(matches!(
        response.result,
        Err(TurnFailure {
            kind: TurnFailureKind::MissingCore,
            ..
        })
    ));
}

#[test]
fn a_missing_provider_fails_the_request_instead_of_losing_it() {
    let mut app = App::new();
    spawn_agent(&mut app);
    app.add_plugin(SessionPlugin)
        .add_plugin(ModelPlugin)
        .add_plugin(ToolsPlugin::empty())
        .add_plugin(AgentPlugin)
        .insert_resource(RequestBuilderResource::new(Box::new(
            StandardRequestBuilder,
        )))
        .insert_resource(Capture::default())
        .add_systems(Update, capture_finished)
        .add_plugin(TokioPlugin);
    let agent = agent(&app);
    send_request(&mut app, agent, "hello");
    app.pump().expect("the schedule runs without a provider");

    let capture = capture(&app);
    assert_eq!(capture.finished.len(), 1);
    assert!(matches!(
        &capture.finished[0],
        Err(failure) if failure.kind == TurnFailureKind::Model
    ));
}

#[test]
fn rotating_a_session_clears_its_log_and_resets_durability() {
    let mut app = app(ProviderState::default());
    let agent = agent(&app);
    app.world_mut()
        .get_mut::<AgentCore>(agent)
        .expect("core state")
        .log_mut()
        .record(Event::TurnStart);
    {
        let mut durability = app
            .world_mut()
            .get_mut::<Durability>(agent)
            .expect("durability component");
        durability.sent_seq = 3;
        durability.flushed_seq = 3;
    }

    app.world_mut().send_message(RotateSession { agent });
    app.pump().expect("schedule runs");

    assert!(
        app.world()
            .get::<AgentCore>(agent)
            .expect("core state")
            .log()
            .events()
            .is_empty(),
        "the in-memory log is cleared"
    );
    let durability = app
        .world()
        .get::<Durability>(agent)
        .expect("durability component")
        .clone();
    assert_eq!(durability.sent_seq, 0, "the durability baseline is reset");
    assert_eq!(durability.flushed_seq, 0);
    assert!(durability.is_durable());
}

#[test]
fn losing_core_state_fails_the_turn_and_its_queued_requests() {
    let mut app = app(ProviderState {
        muted: true,
        ..Default::default()
    });
    let agent = agent(&app);
    send_request(&mut app, agent, "first");
    app.pump().expect("schedule runs");

    // The first turn is waiting for a reply; the second request queues.
    send_request(&mut app, agent, "second");
    app.pump().expect("schedule runs");

    // The host removes the core state while the turn is in flight.
    app.world_mut().remove::<AgentCore>(agent);
    app.pump().expect("the schedule keeps running");

    let capture = capture(&app);
    assert_eq!(capture.finished.len(), 2, "active turn + queued request");
    assert!(
        capture.finished.iter().all(
            |result| matches!(result, Err(failure) if failure.kind == TurnFailureKind::MissingCore)
        ),
        "both failures are typed: {:?}",
        capture.finished
    );
}

#[tokio::test]
async fn an_incomplete_agent_does_not_block_a_complete_one() {
    let mut app = app(ProviderState::default());
    let complete = agent(&app);
    let incomplete = app.world_mut().spawn_empty();

    send_request(&mut app, incomplete, "bad");
    send_request(&mut app, complete, "good");

    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if capture(&app).finished.len() >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }

    let capture = capture(&app);
    assert_eq!(capture.finished.len(), 2);
    assert!(
        capture.finished.iter().any(|result| matches!(
            result,
            Err(failure) if failure.kind == TurnFailureKind::MissingCore
        )),
        "the incomplete agent fails its turn: {:?}",
        capture.finished
    );
    assert!(
        capture.finished.contains(&Ok(Outcome::Answered)),
        "the complete agent still answers: {:?}",
        capture.finished
    );
}

#[test]
fn a_user_message_to_a_non_agent_entity_is_dropped_without_panicking() {
    let mut app = app(ProviderState::default());
    let stray = app.world_mut().spawn_empty();
    app.world_mut().send_message(UserMessage {
        agent: stray,
        content: Content::try_from("hello").expect("static content is valid"),
    });
    app.pump().expect("the schedule runs");

    assert!(capture(&app).finished.is_empty());
}

#[tokio::test]
async fn cancelling_a_queued_request_settles_it() {
    let mut app = app(ProviderState::muted());
    let agent = agent(&app);
    let seq = seq_of(&app);
    let sender = app.runtime_sender();
    let first = seq.next();
    let second = seq.next();
    sender
        .send_message(TurnRequested {
            id: first,
            agent,
            config: config("first"),
            responder: None,
        })
        .expect("runtime is alive");
    sender
        .send_message(TurnRequested {
            id: second,
            agent,
            config: config("second"),
            responder: None,
        })
        .expect("runtime is alive");
    app.pump()
        .expect("the first turn starts; the second queues");

    sender
        .send_message(CancelTurn { request: second })
        .expect("runtime is alive");
    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if capture(&app).ids.contains(&second) {
            let capture = capture(&app);
            let failure = capture
                .ids
                .iter()
                .position(|id| *id == second)
                .map(|index| &capture.finished[index])
                .expect("the cancelled request has a response");
            assert!(matches!(
                failure,
                Err(TurnFailure {
                    kind: TurnFailureKind::Cancelled,
                    ..
                })
            ));
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the cancelled request did not finish");
}

#[tokio::test]
async fn request_turn_awaits_exactly_one_response() {
    let mut app = app(ProviderState::default());
    let agent = agent(&app);
    let seq = seq_of(&app);
    let sender = app.runtime_sender();
    let task = tokio::spawn(async move {
        crate::host::request_turn(&sender, &seq, agent, config("hello")).await
    });

    let mut response = None;
    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if task.is_finished() {
            response = Some(task.await.expect("the request task joins"));
            break;
        }
        tokio::task::yield_now().await;
    }
    let response = response.expect("the request finishes").expect("a response");
    assert!(matches!(
        response.result,
        Ok(ref result) if result.outcome == Outcome::Answered
    ));
}

#[tokio::test]
async fn a_pushed_response_arrives_exactly_once_and_the_entity_is_reaped() {
    let mut app = app(ProviderState::default());
    let agent = agent(&app);
    let seq = seq_of(&app);
    let (tx, rx) = async_channel::bounded(1);
    let id = seq.next();
    app.world_mut().send_message(TurnRequested {
        id,
        agent,
        config: config("hello"),
        responder: Some(TurnResponder::new(tx)),
    });

    let mut received = None;
    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if let Ok(response) = rx.try_recv() {
            received = Some(response);
            break;
        }
        tokio::task::yield_now().await;
    }
    let response = received.expect("the responder receives the response");
    assert_eq!(response.id, id);
    assert!(matches!(response.result, Ok(ref result) if result.outcome == Outcome::Answered));

    // 送达后的下一个 tick，推送式请求实体被回收。
    for _ in 0..4 {
        app.pump().expect("schedule runs");
        tokio::task::yield_now().await;
    }
    assert!(
        app.world().entity_with::<TurnResponse>().is_none(),
        "a pushed request is reaped instead of lingering"
    );
    assert!(rx.try_recv().is_err(), "exactly one response is pushed");
}

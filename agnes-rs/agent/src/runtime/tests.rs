//! 消息管线测试：脚本化的 provider 与工具驱动完整 turn，覆盖正常往返、
//! 工具轮、门禁、审批、打断与失败收场。

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
    App, Entity, MessageReader, MessageWriter, Query, Res, ResMut, RuntimeSender, Update,
};
use agnes_io::http::TransportError;
use agnes_io::llm::{LlmCall, LlmCancel, LlmError, LlmProvider, LlmReply};
use agnes_io::tool::Tool;
use agnes_plugin_async::TokioPlugin;
use agnes_plugin_session::{SessionPlugin, session_state};
use agnes_record::{
    Durability, Event, HostEvent, RunEvent, RunEventKind, SessionError, SessionStore,
    SessionStoreResource,
};
use agnes_util::NonEmpty;
use async_trait::async_trait;
use jiff::Timestamp;

use super::messages::{ApprovalDecided, InterruptTurn, TurnFinished, TurnRequested, UserMessage};
use super::{
    ActiveTurn, AgentPlugin, PendingTurns, PendingUserMessages, RequestBuilderResource, ToolsPlugin,
};
use crate::AgentRoot;
use crate::StandardRequestBuilder;
use crate::approval::ApprovalChannel;
use crate::model::{ModelPlugin, ModelSettings, SetModel, SetThinking};
use crate::permission::{PermissionRules, Rule};

const MODEL: &str = "test-model";

/// The host-spawned agent root.
fn agent(app: &App) -> Entity {
    app.world()
        .entity_with::<crate::AgentRoot>()
        .expect("agent root exists")
}

fn capture(app: &App) -> Capture {
    app.world()
        .resource::<Capture>()
        .expect("capture is installed")
        .clone()
}

#[derive(Default, Clone)]
struct Capture {
    finished: Vec<Result<Outcome, String>>,
    run_events: Vec<RunEvent>,
}

fn capture_finished(mut reader: MessageReader<TurnFinished>, mut capture: ResMut<Capture>) {
    for message in reader.read() {
        capture.finished.push(match &message.result {
            Ok(result) => Ok(result.outcome),
            Err(failure) => Err(failure.cause.clone()),
        });
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

/// 宿主装配：spawn 一个带完整会话状态的 agent 根实体。
fn spawn_agent(app: &mut App) -> Entity {
    let (log, pump, durability) = session_state(&[]).expect("session state");
    app.world_mut().spawn_bundle((
        AgentRoot,
        log,
        pump,
        durability,
        ModelSettings::new(
            ModelName::from_str(MODEL).expect("static name is valid"),
            Thinking::Off,
        ),
        PendingTurns::default(),
        PendingUserMessages::default(),
    ))
}

fn app(provider: ProviderState) -> App {
    app_with(provider, ToolsPlugin::empty())
}

/// 只装插件、不 spawn agent 根实体：用于装配校验测试。
fn bare_app() -> App {
    let mut app = App::new();
    app.add_plugin(SessionPlugin)
        .add_plugin(ModelPlugin)
        .add_plugin(ToolsPlugin::empty())
        .add_plugin(AgentPlugin)
        .insert_resource(LlmProvider)
        .insert_resource(RequestBuilderResource::new(Box::new(
            StandardRequestBuilder,
        )))
        .insert_resource(Capture::default())
        .add_systems(Update, capture_finished)
        .add_plugin(TokioPlugin);
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
    let capture = drive(app, |sender| {
        sender
            .send_message(TurnRequested {
                agent,
                config: config("hello"),
            })
            .expect("runtime is alive");
    })
    .await;

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
    app.world_mut().send_message(SetModel {
        agent,
        model: ModelName::from_str("other-model").expect("static name is valid"),
    });
    app.world_mut().send_message(SetThinking {
        agent,
        thinking: Thinking::On(ReasoningEffort::Low),
    });
    app.pump().expect("settings apply");

    let capture = drive(app, |sender| {
        sender
            .send_message(TurnRequested {
                agent,
                config: config("hello"),
            })
            .expect("runtime is alive");
    })
    .await;

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
            agent,
            config: config("hold"),
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
    let mut app = app(ProviderState::default());
    let agent = agent(&app);
    let sender = app.runtime_sender();
    for goal in ["first", "second"] {
        sender
            .send_message(TurnRequested {
                agent,
                config: config(goal),
            })
            .expect("runtime is alive");
    }
    for _ in 0..512 {
        app.pump().expect("schedule runs");
        if capture(&app).finished.len() == 2 {
            let capture = capture(&app);
            assert_eq!(
                capture.finished,
                vec![Ok(Outcome::Answered), Ok(Outcome::Answered)]
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

    // 第二个 agent 根实体：宿主按同样的流程 spawn 并挂上每会话状态。
    let first = agent(&app);
    let second = spawn_agent(&mut app);

    let sender = app.runtime_sender();
    for agent in [first, second] {
        sender
            .send_message(TurnRequested {
                agent,
                config: config("hold"),
            })
            .expect("runtime is alive");
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
    let capture = drive(app, |sender| {
        sender
            .send_message(TurnRequested {
                agent,
                config: config("use the tool"),
            })
            .expect("runtime is alive");
    })
    .await;

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
    let capture = drive(app, |sender| {
        sender
            .send_message(TurnRequested {
                agent,
                config: config("use the tool"),
            })
            .expect("runtime is alive");
    })
    .await;

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
            agent,
            config: config("use the tool"),
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
    let mut app = app_with(
        ProviderState::with_replies(vec![Ok(tool_response())]),
        ToolsPlugin::new(Arc::new(SlowTool::new())),
    );

    let agent = agent(&app);
    let sender = app.runtime_sender();
    sender
        .send_message(TurnRequested {
            agent,
            config: config("use the tool"),
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
            agent,
            config: config("hello"),
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
    let sender = app.runtime_sender();
    for _ in 0..2 {
        sender
            .send_message(TurnRequested {
                agent,
                config: config("hello"),
            })
            .expect("runtime is alive");
    }
    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if capture(&app).finished.len() == 2 {
            let capture = capture(&app);
            assert_eq!(capture.finished.len(), 2);
            for outcome in &capture.finished {
                let cause = outcome.as_ref().expect_err("model fails").clone();
                assert!(cause.contains("timed out"));
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
            agent,
            config: config("hello"),
        })
        .expect("runtime is alive");

    for _ in 0..256 {
        app.pump().expect("schedule runs");
        if !capture(&app).finished.is_empty() {
            assert_eq!(capture(&app).finished, vec![Err("disk full".to_owned())]);
            // 应用存活：下一个 turn 不再发请求，同样立即失败收场。
            sender
                .send_message(TurnRequested {
                    agent,
                    config: config("again"),
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
            agent,
            config: config("hello"),
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
fn agent_plugin_registers_without_an_agent_root() {
    let _app = bare_app();
}

#[test]
fn a_turn_request_to_a_root_missing_model_settings_finishes_with_an_error() {
    let mut app = bare_app();
    let (log, pump, durability) = session_state(&[]).expect("session state");
    let incomplete = app.world_mut().spawn_bundle((
        AgentRoot,
        log,
        pump,
        durability,
        PendingTurns::default(),
        PendingUserMessages::default(),
    ));
    app.world_mut().send_message(TurnRequested {
        agent: incomplete,
        config: config("hello"),
    });
    app.pump().expect("the schedule runs without panicking");

    let capture = capture(&app);
    assert_eq!(capture.finished.len(), 1);
    let Err(cause) = &capture.finished[0] else {
        panic!("the turn must fail: {:?}", capture.finished);
    };
    assert!(cause.contains("core state"), "unexpected cause: {cause}");
}

#[tokio::test]
async fn an_incomplete_agent_does_not_block_a_complete_one() {
    let mut app = app(ProviderState::default());
    let complete = agent(&app);
    let incomplete = app.world_mut().spawn_bundle((
        AgentRoot,
        PendingTurns::default(),
        PendingUserMessages::default(),
    ));

    app.world_mut().send_message(TurnRequested {
        agent: incomplete,
        config: config("bad"),
    });
    app.world_mut().send_message(TurnRequested {
        agent: complete,
        config: config("good"),
    });

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
        capture.finished[0].is_err(),
        "the incomplete agent fails its turn"
    );
    assert_eq!(capture.finished[1], Ok(Outcome::Answered));
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

use super::*;
use crate::approval::ApprovalChannel;
use crate::ext::{Command, Plugin, PluginEvent};
use crate::permission::{PermissionPlugin, PermissionRules, Rule};
use crate::record::{Event as RecordEvent, HostEvent};
use agnes_core::config::{StepBudget, SystemPrompt};
use agnes_core::context::Context;
use agnes_core::header::{Environment, Header};
use agnes_core::llm::{LlmResponse, StopReason};
use agnes_core::message::{Message, ToolArguments, ToolOutput};
use agnes_core::tool::{JsonSchema, ToolError, ToolSpec};
use agnes_io::input::{Input as InputPort, NoInput};
use agnes_io::llm::{ApiBody, Llm};
use agnes_io::tool::Tool;
use agnes_util::NonEmpty;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// 回放一段响应脚本、记录收到的每个请求的模型。
struct ScriptedLlm {
    responses: Mutex<std::vec::IntoIter<LlmResponse>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(self
            .responses
            .lock()
            .unwrap()
            .next()
            .expect("scripted model ran dry"))
    }
}

/// 记参数的工具，总是成功。
struct Recorder {
    spec: ToolSpec,
    calls: Mutex<Vec<String>>,
}
impl Recorder {
    fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                ToolName::from_str("note").unwrap(),
                "records a note",
                JsonSchema::try_from(serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }))
                .unwrap(),
            ),
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Tool for Recorder {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn run(&self, arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
        self.calls
            .lock()
            .unwrap()
            .push(arguments.as_str().to_owned());
        Ok(ToolOutput::success("noted"))
    }
}

/// 运行期由插件注入的工具。
struct InjectedTool {
    spec: ToolSpec,
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl Tool for InjectedTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
        *self.calls.lock().unwrap() += 1;
        Ok(ToolOutput::success("injected done"))
    }
}

/// 收集所有事件的 Sink。
struct Collector(Vec<Event>);

impl Sink for Collector {
    fn event(&mut self, event: &Event) {
        self.0.push(event.clone());
    }
}

fn config_with(tools: Vec<ToolSpec>, budget: u32) -> agnes_core::turn::AgentConfig {
    agnes_core::turn::AgentConfig {
        header: Header::new(SystemPrompt::new("sys"), Environment::new(tools)),
        goal: Content::try_from("take a note").unwrap(),
        budget: StepBudget::new(NonZeroU32::new(budget).unwrap()),
    }
}

fn config(budget: u32) -> agnes_core::turn::AgentConfig {
    config_with(Vec::new(), budget)
}

fn model() -> ModelName {
    ModelName::from_str("test-model").unwrap()
}

fn response(stop_reason: StopReason) -> LlmResponse {
    LlmResponse {
        stop_reason,
        usage: TokenUsage {
            input: 1,
            output: 1,
        },
    }
}

fn completed(text: &str) -> StopReason {
    StopReason::Completed {
        content: Content::try_from(text).unwrap(),
    }
}

fn tool_use(calls: Vec<ToolCall>) -> StopReason {
    StopReason::ToolUse {
        content: None,
        calls: NonEmpty::from_vec(calls).unwrap(),
    }
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: agnes_core::message::ToolCallId::new(id),
        name: ToolName::from_str(name).unwrap(),
        arguments: ToolArguments::new(arguments),
    }
}

/// 驱动上报的核心事件，按序。
fn recorded(events: &[Event]) -> Vec<RecordEvent> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Recorded { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect()
}

/// 事件种类，按序。
fn shape(events: &[RecordEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            RecordEvent::Header(_) => "header",
            RecordEvent::TurnStart => "turn_start",
            RecordEvent::StepStart { .. } => "step_start",
            RecordEvent::MessageStart { .. } => "message_start",
            RecordEvent::Message(_) => "message",
            RecordEvent::MessageEnd { .. } => "message_end",
            RecordEvent::ToolExecutionStart { .. } => "tool_start",
            RecordEvent::ToolExecutionEnd { .. } => "tool_end",
            RecordEvent::StepEnd { .. } => "step_end",
            RecordEvent::MessagesInjected { .. } => "messages_injected",
            RecordEvent::TurnEnd { .. } => "turn_end",
        })
        .collect()
}

fn messages_of(log: &Log) -> Vec<&Message> {
    log.messages().collect()
}

#[tokio::test]
async fn happy_path_runs_tools_and_finishes() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("all done")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    assert_eq!(summary.reply().map(Content::as_str), Some("all done"));
    assert_eq!(summary.steps_used, 2);
    assert_eq!(
        summary.usage,
        TokenUsage {
            input: 2,
            output: 2
        }
    );
    assert_eq!(
        recorder.calls.lock().unwrap().clone(),
        vec![r#"{"text":"hi"}"#.to_owned()]
    );

    // 会话消息：goal、工具调用、工具结果、最终答复
    let messages = messages_of(&summary.log);
    assert_eq!(messages.len(), 4);
    assert!(matches!(messages[0], Message::User { .. }));
    assert!(matches!(messages[3], Message::Assistant(_)));
    assert_eq!(summary.log.header().unwrap().system().as_str(), "sys");

    // 驱动按顺序上报了整个生命周期
    let events = &sink.0;
    assert!(matches!(events[0], Event::Started { .. }));
    assert_eq!(
        shape(&recorded(events)),
        vec![
            "header",
            "message",
            "turn_start",
            "step_start",
            "message_start",
            "message",
            "message_end",
            "tool_start",
            "tool_end",
            "message",
            "step_end",
            "step_start",
            "message_start",
            "message",
            "message_end",
            "step_end",
            "turn_end"
        ]
    );
    assert!(matches!(
        events.last(),
        Some(Event::Finished { outcome, .. })
            if matches!(outcome, Outcome::Answered)
    ));
    // 上报的核心事件就是日志里的那些
    assert_eq!(recorded(events).len(), summary.log.events().len());
    assert!(summary.pending_messages.is_empty());
}

/// 记录落盘事件、审计记录与 flush 次数的会话日志。
struct RecordingSession {
    events: Arc<Mutex<Vec<RecordEvent>>>,
    notes: Arc<Mutex<Vec<HostEvent>>>,
    flushes: Arc<Mutex<usize>>,
}

#[async_trait]
impl crate::SessionLog for RecordingSession {
    async fn append(
        &mut self,
        _at: jiff::Timestamp,
        event: &RecordEvent,
    ) -> Result<(), crate::SessionError> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }

    async fn append_host(
        &mut self,
        _at: jiff::Timestamp,
        event: &HostEvent,
    ) -> Result<(), crate::SessionError> {
        self.notes.lock().unwrap().push(event.clone());
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), crate::SessionError> {
        *self.flushes.lock().unwrap() += 1;
        Ok(())
    }
}

#[tokio::test]
async fn every_reported_event_is_persisted_in_order() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("done")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let notes = Arc::new(Mutex::new(Vec::new()));
    let flushes = Arc::new(Mutex::new(0));
    let mut session = RecordingSession {
        events: Arc::clone(&events),
        notes: Arc::clone(&notes),
        flushes: Arc::clone(&flushes),
    };

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink).with_session(&mut session))
        .await
        .unwrap();

    // 落盘顺序就是上报顺序，且一个不落
    assert_eq!(*events.lock().unwrap(), recorded(&sink.0));
    assert_eq!(events.lock().unwrap().len(), summary.log.events().len());
    // 每个模型调用前 + 收场各一次
    assert!(*flushes.lock().unwrap() >= 2);
}

#[tokio::test]
async fn approval_decisions_are_persisted_as_audit_notes() {
    let (approval, mut questions) = ApprovalChannel::channel(None);
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("done")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(PermissionPlugin::new(PermissionRules {
            deny: vec![],
            ask: vec![Rule::Name(ToolName::from_str("note").unwrap())],
        }))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .approvals(Some(&approval))
        .build()
        .unwrap();

    let notes = Arc::new(Mutex::new(Vec::new()));
    let mut session = RecordingSession {
        events: Arc::new(Mutex::new(Vec::new())),
        notes: Arc::clone(&notes),
        flushes: Arc::new(Mutex::new(0)),
    };
    let reviewer = tokio::spawn(async move {
        let question = questions.recv().await.expect("the question arrives");
        question.reply.send(true).expect("the approval is sent");
    });

    let mut sink = Collector(Vec::new());
    host.run(RunRequest::new(config(8), Log::default(), &mut sink).with_session(&mut session))
        .await
        .unwrap();
    reviewer.await.unwrap();

    let notes = notes.lock().unwrap();
    assert_eq!(notes.len(), 2);
    assert!(matches!(
        &notes[0],
        HostEvent::ApprovalRequested { tool, .. } if tool.as_str() == "note"
    ));
    assert!(matches!(
        &notes[1],
        HostEvent::ApprovalResolved {
            outcome: ApprovalOutcome::AllowedOnce,
            ..
        }
    ));
}

#[tokio::test]
async fn a_failed_run_records_its_cause() {
    struct Broken;
    #[async_trait]
    impl Llm for Broken {
        async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse, LlmError> {
            Err(LlmError::Api {
                status: http::StatusCode::SERVICE_UNAVAILABLE,
                body: ApiBody::new("down", "down"),
            })
        }
    }

    let notes = Arc::new(Mutex::new(Vec::new()));
    let mut session = RecordingSession {
        events: Arc::new(Mutex::new(Vec::new())),
        notes: Arc::clone(&notes),
        flushes: Arc::new(Mutex::new(0)),
    };
    let llm = Broken;
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .build()
        .unwrap();

    let err = host
        .run(
            RunRequest::new(config(4), Log::default(), &mut Collector(Vec::new()))
                .with_session(&mut session),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::Llm(_)));

    let notes = notes.lock().unwrap();
    assert_eq!(notes.len(), 1);
    assert!(matches!(&notes[0], HostEvent::Failed { cause } if cause.contains("down")));
}

#[tokio::test]
async fn tool_failure_is_surfaced_not_fatal() {
    struct AlwaysFails {
        spec: ToolSpec,
    }
    #[async_trait]
    impl Tool for AlwaysFails {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        async fn run(&self, _arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Io {
                action: "cannot read the disk",
                path: "disk".to_owned(),
                source: std::io::Error::other("disk on fire"),
            })
        }
    }

    let tool = Arc::new(AlwaysFails {
        spec: ToolSpec::new(
            ToolName::from_str("boom").unwrap(),
            "",
            JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        ),
    }) as Arc<dyn Tool>;
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "boom", "{}")])),
        response(completed("gave up")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .tool(tool)
        .build()
        .unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink))
        .await
        .unwrap();
    assert!(matches!(summary.outcome, Outcome::Answered));
    assert!(recorded(&sink.0).iter().any(|event| matches!(
        event,
        RecordEvent::Message(Message::ToolResult { output, .. }) if output.is_error()
    )));
}

#[tokio::test]
async fn transport_error_aborts_run() {
    struct Broken;
    #[async_trait]
    impl Llm for Broken {
        async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse, LlmError> {
            Err(LlmError::Api {
                status: http::StatusCode::SERVICE_UNAVAILABLE,
                body: ApiBody::new("down", "down"),
            })
        }
    }
    let llm = Broken;
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .build()
        .unwrap();
    let err = host
        .run(RunRequest::new(
            config(4),
            Log::default(),
            &mut Collector(Vec::new()),
        ))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        AgentError::Llm(LlmError::Api { status, .. })
            if status == http::StatusCode::SERVICE_UNAVAILABLE
    ));
}

/// 对提示词有想法的转换者：丢掉环境里的工具，先垫一条指令。
struct PrimingRequests {
    model: ModelName,
}

impl RequestBuilder for PrimingRequests {
    fn build(&self, context: &Context) -> LlmRequest {
        let mut prompt = context.prompt();
        prompt.tools.clear();
        prompt.input.insert(
            0,
            Message::User {
                content: Content::try_from("<system> be extra brief").unwrap(),
            },
        );
        LlmRequest {
            model: self.model.clone(),
            prompt,
        }
    }
}

#[tokio::test]
async fn the_request_builder_shapes_what_the_model_sees() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![response(completed("ok"))]);
    let mut host = Host::builder(&llm, &NoInput)
        .request_builder(Box::new(PrimingRequests { model: model() }))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();

    let config = config_with(host.tools().specs(), 4);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();
    assert!(matches!(summary.outcome, Outcome::Answered));

    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    // 标准投影本会带上 recorder；这个转换者把工具丢掉，改为垫话
    assert!(requests[0].prompt.tools.is_empty());
    assert!(matches!(
        &requests[0].prompt.input[0],
        Message::User { content } if content.as_str() == "<system> be extra brief"
    ));
    // 环境仍在会话里，未被破坏
    assert_eq!(
        summary.log.header().unwrap().environment().tools().count(),
        1
    );
}

/// 测试直接驱动的输入：消息队列 + 停止通道。
struct FakeInput {
    messages: Mutex<VecDeque<Content>>,
    stop: watch::Sender<bool>,
}

impl FakeInput {
    fn new() -> Self {
        Self {
            messages: Mutex::new(VecDeque::new()),
            stop: watch::channel(false).0,
        }
    }

    fn queue(&self, text: &str) {
        self.messages
            .lock()
            .unwrap()
            .push_back(Content::try_from(text).unwrap());
    }

    fn interrupt(&self) {
        self.stop.send_replace(true);
    }
}

#[async_trait]
impl InputPort for FakeInput {
    fn drain(&self) -> Vec<Content> {
        self.messages.lock().unwrap().drain(..).collect()
    }

    async fn interrupted(&self) {
        let mut stop = self.stop.subscribe();
        while !*stop.borrow_and_update() {
            if stop.changed().await.is_err() {
                return;
            }
        }
    }
}

/// 永不返回的模型；用户打断时 future 被就地丢弃。
struct HangingLlm;

#[async_trait]
impl Llm for HangingLlm {
    async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        std::future::pending().await
    }
}

/// 永不返回的工具。
struct HangingTool {
    spec: ToolSpec,
}

#[async_trait]
impl Tool for HangingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn run(&self, _arguments: &ToolArguments) -> Result<ToolOutput, ToolError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn queued_user_message_joins_the_first_call() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("all done")),
    ]);
    let input = FakeInput::new();
    input.queue("also update the readme");
    let mut host = Host::builder(&llm, &input)
        .model(model())
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    assert_eq!(summary.reply().map(Content::as_str), Some("all done"));
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    // 第一次调用前排空并注入：system, goal, injected user
    let first = &requests[0].prompt.input;
    assert_eq!(first.len(), 3);
    assert!(matches!(
        &first[2],
        Message::User { content } if content.as_str() == "also update the readme"
    ));
    // 第二次请求带着完整历史：tool result 在注入消息之后
    let second = &requests[1].prompt.input;
    assert_eq!(second.len(), 5);
    assert!(matches!(&second[4], Message::ToolResult { .. }));
    // 注入事件随日志上报
    assert!(recorded(&sink.0).iter().any(|event| matches!(
        event,
        RecordEvent::MessagesInjected { count } if *count == 1
    )));
    // 注入的消息属于会话上下文
    let messages = messages_of(&summary.log);
    assert_eq!(messages.len(), 5);
    assert!(matches!(
        messages[1],
        Message::User { content } if content.as_str() == "also update the readme"
    ));
    assert!(summary.pending_messages.is_empty());
}

#[tokio::test]
async fn interrupt_during_a_model_call_stops_the_run() {
    let input = FakeInput::new();
    let llm = HangingLlm;
    let mut host = Host::builder(&llm, &input).model(model()).build().unwrap();
    let mut sink = Collector(Vec::new());
    // 先让 run 到达挂起的模型调用，再打断
    let interrupter = async {
        tokio::task::yield_now().await;
        input.interrupt();
    };
    let (result, ()) = tokio::join!(
        host.run(RunRequest::new(config(4), Log::default(), &mut sink)),
        interrupter
    );
    let summary = result.unwrap();

    assert_eq!(summary.outcome, Outcome::Interrupted);
    assert_eq!(summary.steps_used, 1);
    assert_eq!(summary.usage, TokenUsage::default());
    // 只剩 goal —— 被丢弃的响应从未入账
    assert_eq!(messages_of(&summary.log).len(), 1);
    assert!(matches!(messages_of(&summary.log)[0], Message::User { .. }));
    // turn 被干净地收场，不是悬着
    assert_eq!(
        shape(summary.log.events()),
        vec![
            "header",
            "message",
            "turn_start",
            "step_start",
            "message_start",
            "step_end",
            "turn_end"
        ]
    );
    assert!(matches!(
        summary.log.events().last(),
        Some(RecordEvent::TurnEnd {
            outcome: Outcome::Interrupted,
            ..
        })
    ));
}

#[tokio::test]
async fn interrupt_during_tools_settles_the_whole_batch() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![response(tool_use(vec![
        call("c1", "note", r#"{"text":"hi"}"#),
        call("c2", "hang", "{}"),
    ]))]);
    let input = FakeInput::new();
    let mut host = Host::builder(&llm, &input)
        .model(model())
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .tool(Arc::new(HangingTool {
            spec: ToolSpec::new(
                ToolName::from_str("hang").unwrap(),
                "never returns",
                JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
            ),
        }))
        .build()
        .unwrap();
    let mut sink = Collector(Vec::new());
    // 第一个调用跑完、第二个挂起时打断
    let interrupter = async {
        tokio::task::yield_now().await;
        input.interrupt();
    };
    let (result, ()) = tokio::join!(
        host.run(RunRequest::new(config(8), Log::default(), &mut sink)),
        interrupter
    );
    let summary = result.unwrap();

    assert_eq!(summary.outcome, Outcome::Interrupted);
    assert_eq!(summary.steps_used, 1);
    assert_eq!(llm.requests().len(), 1); // 没有后续模型调用
    assert_eq!(recorder.calls.lock().unwrap().len(), 1);
    let messages = messages_of(&summary.log);
    // goal, assistant (tool calls), tool result, interrupted tool result
    assert_eq!(messages.len(), 4);
    assert!(matches!(
        messages[2],
        Message::ToolResult { output, .. } if !output.is_error()
    ));
    assert!(matches!(
        messages[3],
        Message::ToolResult { output, .. }
            if output.is_error() && output.content().contains("interrupted")
    ));
}

#[tokio::test]
async fn a_queued_message_joins_the_only_allowed_step() {
    let input = FakeInput::new();
    input.queue("only for this step");
    let llm = ScriptedLlm::new(vec![response(completed("all done"))]);
    let mut host = Host::builder(&llm, &input).model(model()).build().unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(1), Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    assert_eq!(summary.reply().map(Content::as_str), Some("all done"));
    // 唯一允许的一步在调用前排空注入，因此消息进了请求与会话。
    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].prompt.input.iter().any(
        |message| matches!(message, Message::User { content } if content.as_str() == "only for this step")
    ));
    assert_eq!(summary.steps_used, 1);
    assert!(summary.pending_messages.is_empty());
    // goal、注入消息、答复
    assert_eq!(messages_of(&summary.log).len(), 3);
    assert!(recorded(&sink.0).iter().any(|event| matches!(
        event,
        RecordEvent::MessagesInjected { count } if *count == 1
    )));
}

#[tokio::test]
async fn a_queued_message_joins_the_first_call_before_the_budget_runs_out() {
    let recorder = Arc::new(Recorder::new());
    let input = FakeInput::new();
    input.queue("also check the tests");
    let llm = ScriptedLlm::new(vec![response(tool_use(vec![call(
        "c1",
        "note",
        r#"{"text":"hi"}"#,
    )]))]);
    let mut host = Host::builder(&llm, &input)
        .model(model())
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(1), Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::BudgetExhausted);
    assert_eq!(summary.steps_used, 1);
    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].prompt.input.iter().any(
        |message| matches!(message, Message::User { content } if content.as_str() == "also check the tests")
    ));
    assert!(summary.pending_messages.is_empty());
    // goal、注入消息、工具调用、工具结果
    assert_eq!(messages_of(&summary.log).len(), 4);
    assert!(recorded(&sink.0).iter().any(|event| matches!(
        event,
        RecordEvent::MessagesInjected { count } if *count == 1
    )));
}

struct QueueingModel<'a> {
    input: &'a FakeInput,
}

#[async_trait]
impl Llm for QueueingModel<'_> {
    async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        self.input.queue("typed during the final answer");
        Ok(response(completed("done")))
    }
}

#[tokio::test]
async fn a_message_queued_during_the_final_call_is_not_dropped() {
    let input = FakeInput::new();
    let llm = QueueingModel { input: &input };
    let mut host = Host::builder(&llm, &input).model(model()).build().unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(4), Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    assert_eq!(summary.reply().map(Content::as_str), Some("done"));
    // 收场后不再 drain：消息留在 Input 队列，下一次 run 会取到。
    assert!(summary.pending_messages.is_empty());
    assert_eq!(
        input.drain(),
        vec![Content::try_from("typed during the final answer").unwrap()]
    );
}

struct FailingAudit;

#[async_trait]
impl crate::SessionLog for FailingAudit {
    async fn append(
        &mut self,
        _at: jiff::Timestamp,
        _event: &RecordEvent,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }

    async fn append_host(
        &mut self,
        _at: jiff::Timestamp,
        _event: &HostEvent,
    ) -> Result<(), crate::SessionError> {
        Err(crate::SessionError::Io(std::io::Error::other("audit down")))
    }

    async fn flush(&mut self) -> Result<(), crate::SessionError> {
        Ok(())
    }
}

#[tokio::test]
async fn an_audit_write_failure_is_reported_with_the_original_cause() {
    struct Broken;
    #[async_trait]
    impl Llm for Broken {
        async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse, LlmError> {
            Err(LlmError::Api {
                status: http::StatusCode::SERVICE_UNAVAILABLE,
                body: ApiBody::new("down", "down"),
            })
        }
    }

    let llm = Broken;
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .build()
        .unwrap();
    let mut sink = Collector(Vec::new());
    let mut session = FailingAudit;
    let err = host
        .run(RunRequest::new(config(4), Log::default(), &mut sink).with_session(&mut session))
        .await
        .unwrap_err();

    assert!(matches!(err, AgentError::Audit { .. }));
    assert!(
        sink.0
            .iter()
            .any(|event| matches!(event, Event::Failed { .. }))
    );
}

// —— 插件：权限链驱动宿主 FSM ——

/// 记录看到的事件、按配置挡回结果。
struct Witness {
    replacement: Option<String>,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

impl Witness {
    fn new(replacement: Option<&str>, seen: &Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            replacement: replacement.map(str::to_owned),
            seen: Arc::clone(seen),
        }
    }
}

impl Plugin for Witness {
    fn name(&self) -> &'static str {
        "witness"
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        match event {
            PluginEvent::Record(_) => {}
            PluginEvent::ToolCallRequested { .. } => {
                self.seen.lock().unwrap().push("requested");
            }
            PluginEvent::ToolCallFinished { call, .. } => {
                self.seen.lock().unwrap().push("finished");
                if let Some(feedback) = &self.replacement {
                    return vec![Command::ReplaceResult {
                        call: call.id.clone(),
                        feedback: feedback.clone(),
                    }];
                }
            }
            PluginEvent::ToolCallGated { .. } => self.seen.lock().unwrap().push("gated"),
            PluginEvent::ToolResultBlocked { .. } => {
                self.seen.lock().unwrap().push("blocked");
            }
            PluginEvent::ApprovalRequested { .. } => {
                self.seen.lock().unwrap().push("asked");
            }
            PluginEvent::ApprovalResolved { .. } => {
                self.seen.lock().unwrap().push("decided");
            }
        }
        Vec::new()
    }
}

#[derive(Clone, Copy)]
enum Trigger {
    TurnStart,
    StepStart,
    Requested,
    Finished,
    ApprovalRequested,
    TurnEnd,
}

impl Trigger {
    fn matches(self, event: &PluginEvent<'_>) -> bool {
        matches!(
            (self, event),
            (Self::TurnStart, PluginEvent::Record(RecordEvent::TurnStart))
                | (
                    Self::StepStart,
                    PluginEvent::Record(RecordEvent::StepStart { .. })
                )
                | (Self::Requested, PluginEvent::ToolCallRequested { .. })
                | (Self::Finished, PluginEvent::ToolCallFinished { .. })
                | (
                    Self::ApprovalRequested,
                    PluginEvent::ApprovalRequested { .. }
                )
                | (
                    Self::TurnEnd,
                    PluginEvent::Record(RecordEvent::TurnEnd { .. })
                )
        )
    }
}

/// 在指定事件上中止一次。
struct Aborter {
    trigger: Trigger,
    fired: bool,
}

impl Aborter {
    fn new(trigger: Trigger) -> Self {
        Self {
            trigger,
            fired: false,
        }
    }
}

impl Plugin for Aborter {
    fn name(&self) -> &'static str {
        "aborter"
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        if self.fired || !self.trigger.matches(event) {
            return Vec::new();
        }
        self.fired = true;
        vec![Command::Abort {
            reason: "policy says stop".into(),
        }]
    }
}

/// 在指定事件上入队一条注入。
struct Injector {
    trigger: Trigger,
    content: &'static str,
    fired: bool,
}

impl Injector {
    fn new(trigger: Trigger, content: &'static str) -> Self {
        Self {
            trigger,
            content,
            fired: false,
        }
    }
}

impl Plugin for Injector {
    fn name(&self) -> &'static str {
        "injector"
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        if self.fired || !self.trigger.matches(event) {
            return Vec::new();
        }
        self.fired = true;
        vec![Command::InjectMessage {
            content: Content::try_from(self.content).unwrap(),
        }]
    }
}

/// 在指定事件上注入一个工具。
struct AddToolOn {
    trigger: Trigger,
    tool: Arc<dyn Tool>,
    fired: bool,
}

impl AddToolOn {
    fn new(trigger: Trigger, tool: Arc<dyn Tool>) -> Self {
        Self {
            trigger,
            tool,
            fired: false,
        }
    }
}

impl Plugin for AddToolOn {
    fn name(&self) -> &'static str {
        "tool-injector"
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        if self.fired || !self.trigger.matches(event) {
            return Vec::new();
        }
        self.fired = true;
        vec![Command::AddTool {
            tool: Arc::clone(&self.tool),
        }]
    }
}

/// 在指定事件上移除一个工具。
struct RemoveToolOn {
    trigger: Trigger,
    name: ToolName,
    fired: bool,
}

impl RemoveToolOn {
    fn new(trigger: Trigger, name: ToolName) -> Self {
        Self {
            trigger,
            name,
            fired: false,
        }
    }
}

impl Plugin for RemoveToolOn {
    fn name(&self) -> &'static str {
        "tool-remover"
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        if self.fired || !self.trigger.matches(event) {
            return Vec::new();
        }
        self.fired = true;
        vec![Command::RemoveTool {
            name: self.name.clone(),
        }]
    }
}

#[tokio::test]
async fn an_accepted_result_is_observed_as_finished() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("ok")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(Witness::new(None, &seen))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink))
        .await
        .unwrap();

    assert!(
        matches!(&messages_of(&summary.log)[2], Message::ToolResult { output, .. } if !output.is_error())
    );
    assert_eq!(*seen.lock().unwrap(), vec!["requested", "finished"]);
}

#[tokio::test]
async fn a_blocked_result_is_replaced_by_feedback() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("ok")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(Witness::new(Some("the note was rejected"), &seen))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink))
        .await
        .unwrap();

    // 工具确实跑过；入账的是反馈，仍然是那一个调用的一条结果
    assert_eq!(recorder.calls.lock().unwrap().len(), 1);
    assert!(
        matches!(&messages_of(&summary.log)[2], Message::ToolResult { output, .. }
            if output.is_error() && output.content().contains("the note was rejected"))
    );
    assert!(recorded(&sink.0).iter().any(|event| matches!(
        event,
        RecordEvent::Message(Message::ToolResult { output, .. }) if output.is_error()
    )));
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["requested", "finished", "blocked"]
    );
    assert!(matches!(summary.outcome, Outcome::Answered));
}

#[tokio::test]
async fn denied_tool_never_runs_and_the_model_carries_on() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("understood — moving on")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(PermissionPlugin::new(PermissionRules {
            deny: vec![Rule::Name(ToolName::from_str("note").unwrap())],
            ask: vec![],
        }))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink))
        .await
        .unwrap();

    // 工具没跑
    assert!(recorder.calls.lock().unwrap().is_empty());
    // core 日志里是一次普通的失败工具
    let messages = messages_of(&summary.log);
    assert_eq!(messages.len(), 4);
    assert!(matches!(&messages[2], Message::ToolResult { output, .. }
            if output.is_error() && output.content().contains("denied by policy")));
    // 宿主上报了 gating
    assert!(
        sink.0
            .iter()
            .any(|event| matches!(event, Event::ToolGated { denied, .. }
            if denied.by == "permission" && denied.reason.contains("denied by policy")))
    );
    assert!(matches!(summary.outcome, Outcome::Answered));
    // 事件序列与一次真实工具往返一致（core 视角无法区分拒绝）
    assert_eq!(
        shape(&recorded(&sink.0)),
        vec![
            "header",
            "message",
            "turn_start",
            "step_start",
            "message_start",
            "message",
            "message_end",
            "tool_start",
            "tool_end",
            "message",
            "step_end",
            "step_start",
            "message_start",
            "message",
            "message_end",
            "step_end",
            "turn_end"
        ]
    );
}

#[tokio::test]
async fn an_ask_resolves_by_approval() {
    let (approval, mut questions) = ApprovalChannel::channel(None);
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("did it")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(PermissionPlugin::new(PermissionRules {
            deny: vec![],
            ask: vec![Rule::Name(ToolName::from_str("note").unwrap())],
        }))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .approvals(Some(&approval))
        .build()
        .unwrap();
    // 审批界面接问询、送回批准
    let reviewer = tokio::spawn(async move {
        let question = questions.recv().await.expect("the question arrives");
        assert_eq!(question.tool_name.as_str(), "note");
        assert_eq!(question.call_id.as_str(), "c1");
        question.reply.send(true).expect("the approval is sent");
    });

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink))
        .await
        .unwrap();
    reviewer.await.unwrap();

    // 批准后工具真正运行了
    assert_eq!(
        recorder.calls.lock().unwrap().clone(),
        vec![r#"{"text":"hi"}"#.to_owned()]
    );
    assert!(sink.0.iter().any(
        |event| matches!(event, Event::ApprovalRequested { call, .. } if call.id.as_str() == "c1")
    ));
    assert!(sink.0.iter().any(|event|
        matches!(event, Event::ApprovalResolved { call_id, outcome: ApprovalOutcome::AllowedOnce, .. }
            if call_id.as_str() == "c1")
    ));
    assert!(matches!(summary.outcome, Outcome::Answered));
}

#[tokio::test]
async fn an_ask_refusal_synthesizes_a_failed_result() {
    let (approval, mut questions) = ApprovalChannel::channel(None);
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("refused and seen")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(PermissionPlugin::new(PermissionRules {
            deny: vec![],
            ask: vec![Rule::Name(ToolName::from_str("note").unwrap())],
        }))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .approvals(Some(&approval))
        .build()
        .unwrap();
    let reviewer = tokio::spawn(async move {
        let question = questions.recv().await.expect("the question arrives");
        question.reply.send(false).expect("the refusal is sent");
    });

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink))
        .await
        .unwrap();
    reviewer.await.unwrap();

    assert!(recorder.calls.lock().unwrap().is_empty());
    let messages = messages_of(&summary.log);
    assert!(matches!(&messages[2], Message::ToolResult { output, .. }
            if output.is_error() && output.content().contains("denied by approval: the approval was refused")));
    assert!(sink.0.iter().any(|event| matches!(
        event,
        Event::ApprovalResolved {
            outcome: ApprovalOutcome::Rejected,
            ..
        }
    )));
    assert!(sink.0.iter().any(|event|
        matches!(event, Event::ToolGated { denied, .. } if denied.reason.contains("the approval was refused"))
    ));
    assert!(matches!(summary.outcome, Outcome::Answered));
}

#[tokio::test(start_paused = true)]
async fn interrupting_while_waiting_for_approval_closes_the_run() {
    let (approval, kept_questions) = ApprovalChannel::channel(None);
    // 问询接收端保留但不应答 —— run 卡在审批挂起，直到打断
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![response(tool_use(vec![call(
        "c1",
        "note",
        r#"{"text":"hi"}"#,
    )]))]);
    let input = FakeInput::new();
    let mut host = Host::builder(&llm, &input)
        .model(model())
        .plugin(PermissionPlugin::new(PermissionRules {
            deny: vec![],
            ask: vec![Rule::Name(ToolName::from_str("note").unwrap())],
        }))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .approvals(Some(&approval))
        .build()
        .unwrap();
    let mut sink = Collector(Vec::new());
    // 挂起后才打断：定时器走完一段再触发
    let interrupter = async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        input.interrupt();
    };
    let (result, ()) = tokio::join!(
        host.run(RunRequest::new(config(8), Log::default(), &mut sink)),
        interrupter
    );
    let summary = result.unwrap();
    drop(kept_questions);

    assert_eq!(summary.outcome, Outcome::Interrupted);
    assert_eq!(llm.requests().len(), 1); // 没有后续模型调用
    assert!(recorder.calls.lock().unwrap().is_empty());
    // 问询已记录，落定为未批
    assert!(
        sink.0
            .iter()
            .any(|event| matches!(event, Event::ApprovalRequested { .. }))
    );
    assert!(sink.0.iter().any(|event| matches!(
        event,
        Event::ApprovalResolved {
            outcome: ApprovalOutcome::Cancelled,
            ..
        }
    )));
    // 合成错误结果补位，会话完整
    let messages = messages_of(&summary.log);
    assert_eq!(messages.len(), 3);
    assert!(matches!(&messages[2], Message::ToolResult { output, .. }
            if output.is_error() && output.content().contains("interrupted")));
}

#[tokio::test(start_paused = true)]
async fn an_approval_timeout_denies_the_call() {
    let (approval, kept_questions) = ApprovalChannel::channel(Some(Duration::from_secs(2)));
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("proceeding without it")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(PermissionPlugin::new(PermissionRules {
            deny: vec![],
            ask: vec![Rule::Name(ToolName::from_str("note").unwrap())],
        }))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .approvals(Some(&approval))
        .build()
        .unwrap();
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(8), Log::default(), &mut sink))
        .await
        .unwrap();
    drop(kept_questions);

    assert!(matches!(summary.outcome, Outcome::Answered));
    assert!(recorder.calls.lock().unwrap().is_empty());
    assert!(sink.0.iter().any(|event|
        matches!(event, Event::ToolGated { denied, .. } if denied.reason.contains("timed out"))
    ));
    assert!(sink.0.iter().any(|event| matches!(
        event,
        Event::ApprovalResolved {
            outcome: ApprovalOutcome::Unavailable,
            ..
        }
    )));
    // 模型继续走完
    assert_eq!(llm.requests().len(), 2);
}

#[tokio::test]
async fn a_plugin_injection_reaches_the_next_request() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("done")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(Injector::new(Trigger::Finished, "remember the note"))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();
    let config = config_with(host.tools().specs(), 8);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].prompt.input.iter().any(
        |message| matches!(message, Message::User { content } if content.as_str() == "remember the note")
    ));
    assert!(recorded(&sink.0).iter().any(|event| matches!(
        event,
        RecordEvent::MessagesInjected { count } if *count == 1
    )));
    assert!(summary.pending_messages.is_empty());
}

#[tokio::test]
async fn an_injection_after_the_turn_closes_is_returned_to_the_host() {
    let llm = ScriptedLlm::new(vec![response(completed("done"))]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(Injector::new(Trigger::TurnEnd, "too late"))
        .build()
        .unwrap();
    let config = config_with(host.tools().specs(), 4);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    assert_eq!(
        summary.pending_messages,
        vec![Message::User {
            content: Content::try_from("too late").unwrap()
        }]
    );
    assert!(
        !recorded(&sink.0)
            .iter()
            .any(|event| matches!(event, RecordEvent::MessagesInjected { .. }))
    );
}

#[tokio::test]
async fn abort_on_turn_start_closes_before_the_first_call() {
    let llm = ScriptedLlm::new(vec![response(completed("should not be called"))]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(Aborter::new(Trigger::TurnStart))
        .build()
        .unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(4), Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Interrupted);
    assert_eq!(summary.steps_used, 0);
    assert!(llm.requests().is_empty());
    assert_eq!(
        summary.aborted.as_ref().map(|aborted| aborted.by),
        Some("aborter")
    );
    assert_eq!(
        shape(&recorded(&sink.0)),
        vec!["header", "message", "turn_start", "turn_end"]
    );
}

#[tokio::test]
async fn abort_on_step_start_stops_before_the_model_call() {
    let llm = ScriptedLlm::new(vec![response(completed("should not be called"))]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(Aborter::new(Trigger::StepStart))
        .build()
        .unwrap();

    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config(4), Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Interrupted);
    assert!(llm.requests().is_empty());
    assert_eq!(
        summary.aborted.as_ref().map(|aborted| aborted.by),
        Some("aborter")
    );
    assert_eq!(
        shape(&recorded(&sink.0)),
        vec![
            "header",
            "message",
            "turn_start",
            "step_start",
            "message_start",
            "step_end",
            "turn_end"
        ]
    );
}

#[tokio::test]
async fn abort_before_dispatch_never_runs_the_tool() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![response(tool_use(vec![call(
        "c1",
        "note",
        r#"{"text":"hi"}"#,
    )]))]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(Aborter::new(Trigger::Requested))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();
    let notes = Arc::new(Mutex::new(Vec::new()));
    let mut session = RecordingSession {
        events: Arc::new(Mutex::new(Vec::new())),
        notes: Arc::clone(&notes),
        flushes: Arc::new(Mutex::new(0)),
    };
    let config = config_with(host.tools().specs(), 8);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink).with_session(&mut session))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Interrupted);
    assert_eq!(
        summary.aborted.as_ref().map(|aborted| aborted.by),
        Some("aborter")
    );
    assert!(recorder.calls.lock().unwrap().is_empty());
    assert_eq!(llm.requests().len(), 1);
    assert!(
        sink.0
            .iter()
            .any(|event| matches!(event, Event::Aborted { by, .. } if *by == "aborter"))
    );
    assert!(
        notes
            .lock()
            .unwrap()
            .iter()
            .any(|note| matches!(note, HostEvent::Aborted { by, .. } if by == "aborter"))
    );
    // 未执行的调用仍有一条 not_run 结果，会话完整。
    assert!(matches!(
        messages_of(&summary.log).last(),
        Some(Message::ToolResult { output, .. }) if output.is_error()
    ));
}

#[tokio::test]
async fn abort_after_the_tool_result_keeps_the_result() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![response(tool_use(vec![call(
        "c1",
        "note",
        r#"{"text":"hi"}"#,
    )]))]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(Aborter::new(Trigger::Finished))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();
    let config = config_with(host.tools().specs(), 8);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Interrupted);
    assert_eq!(
        summary
            .aborted
            .as_ref()
            .map(|aborted| aborted.reason.as_str()),
        Some("policy says stop")
    );
    // 工具确实跑过，真实结果先落账，然后才收场。
    assert_eq!(recorder.calls.lock().unwrap().len(), 1);
    assert_eq!(llm.requests().len(), 1);
    assert!(matches!(
        &messages_of(&summary.log)[2],
        Message::ToolResult { output, .. } if !output.is_error()
    ));
}

#[tokio::test]
async fn abort_on_the_approval_question_skips_the_wait() {
    let (approval, questions) = ApprovalChannel::channel(None);
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![response(tool_use(vec![call(
        "c1",
        "note",
        r#"{"text":"hi"}"#,
    )]))]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(PermissionPlugin::new(PermissionRules {
            deny: vec![],
            ask: vec![Rule::Name(ToolName::from_str("note").unwrap())],
        }))
        .plugin(Aborter::new(Trigger::ApprovalRequested))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .approvals(Some(&approval))
        .build()
        .unwrap();
    let notes = Arc::new(Mutex::new(Vec::new()));
    let mut session = RecordingSession {
        events: Arc::new(Mutex::new(Vec::new())),
        notes: Arc::clone(&notes),
        flushes: Arc::new(Mutex::new(0)),
    };
    let config = config_with(host.tools().specs(), 8);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink).with_session(&mut session))
        .await
        .unwrap();
    drop(questions);

    assert_eq!(summary.outcome, Outcome::Interrupted);
    assert!(recorder.calls.lock().unwrap().is_empty());
    assert_eq!(llm.requests().len(), 1);
    let notes = notes.lock().unwrap();
    assert!(
        notes
            .iter()
            .any(|note| matches!(note, HostEvent::ApprovalRequested { .. }))
    );
    assert!(
        notes
            .iter()
            .any(|note| matches!(note, HostEvent::Aborted { .. }))
    );
    assert!(notes.iter().any(|note| matches!(
        note,
        HostEvent::ApprovalResolved {
            outcome: ApprovalOutcome::Cancelled,
            ..
        }
    )));
}

#[tokio::test]
async fn a_plugin_can_add_a_tool_for_the_next_model_call_in_the_same_turn() {
    let recorder = Arc::new(Recorder::new());
    let calls = Arc::new(Mutex::new(0));
    let injected_name = ToolName::from_str("injected").unwrap();
    let injected = Arc::new(InjectedTool {
        spec: ToolSpec::new(
            injected_name.clone(),
            "an injected tool",
            JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        ),
        calls: Arc::clone(&calls),
    });
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(tool_use(vec![call("c2", "injected", "{}")])),
        response(completed("done")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(AddToolOn::new(
            Trigger::Finished,
            Arc::clone(&injected) as Arc<dyn Tool>,
        ))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();
    let config = config_with(host.tools().specs(), 8);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    assert_eq!(*calls.lock().unwrap(), 1);
    let requests = llm.requests();
    assert_eq!(requests.len(), 3);
    let names = |index: usize| {
        requests[index]
            .prompt
            .tools
            .iter()
            .map(|spec| spec.name().as_str().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(0), vec!["note"]);
    assert_eq!(names(1), vec!["injected", "note"]);
    // turn 开局一条 + 工具变更一条
    let events = summary.log.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RecordEvent::Header(_)))
            .count(),
        2
    );
    // 新 header 必须先于下一个 step 开始，请求才与日志里的环境一致。
    let last_header = events
        .iter()
        .rposition(|event| matches!(event, RecordEvent::Header(_)))
        .unwrap();
    let second_step_start = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, RecordEvent::StepStart { .. }))
        .nth(1)
        .map(|(index, _)| index)
        .unwrap();
    assert!(last_header < second_step_start);
    assert!(host.tools().contains(&injected_name));
}

#[tokio::test]
async fn a_plugin_can_remove_a_tool_for_the_next_model_call() {
    let recorder = Arc::new(Recorder::new());
    let note_name = ToolName::from_str("note").unwrap();
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("done")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(RemoveToolOn::new(Trigger::Finished, note_name.clone()))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();
    let config = config_with(host.tools().specs(), 8);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    // 在飞调用按旧注册表执行；下一次请求看不到它。
    assert_eq!(recorder.calls.lock().unwrap().len(), 1);
    assert!(requests[1].prompt.tools.is_empty());
    assert!(!host.tools().contains(&note_name));
    assert!(matches!(
        &messages_of(&summary.log)[2],
        Message::ToolResult { output, .. } if !output.is_error()
    ));
}

#[tokio::test]
async fn a_tool_added_on_turn_end_is_applied_to_the_next_turn() {
    let calls = Arc::new(Mutex::new(0));
    let injected_name = ToolName::from_str("injected").unwrap();
    let injected = Arc::new(InjectedTool {
        spec: ToolSpec::new(
            injected_name.clone(),
            "an injected tool",
            JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        ),
        calls: Arc::clone(&calls),
    });
    let llm = ScriptedLlm::new(vec![response(completed("done"))]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(AddToolOn::new(
            Trigger::TurnEnd,
            Arc::clone(&injected) as Arc<dyn Tool>,
        ))
        .build()
        .unwrap();
    let config = config_with(host.tools().specs(), 4);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    // 已收场的 turn 不补 header，但工具进入注册表供下一 turn 使用。
    assert!(host.tools().contains(&injected_name));
    assert_eq!(
        summary
            .log
            .events()
            .iter()
            .filter(|event| matches!(event, RecordEvent::Header(_)))
            .count(),
        1
    );
    assert_eq!(*calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_duplicate_tool_add_is_a_noop() {
    let recorder = Arc::new(Recorder::new());
    let shadow_calls = Arc::new(Mutex::new(0));
    let shadow = Arc::new(InjectedTool {
        spec: ToolSpec::new(
            ToolName::from_str("note").unwrap(),
            "a shadow of note",
            JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        ),
        calls: Arc::clone(&shadow_calls),
    });
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("done")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(AddToolOn::new(
            Trigger::Finished,
            Arc::clone(&shadow) as Arc<dyn Tool>,
        ))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();
    let config = config_with(host.tools().specs(), 8);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    // 原工具仍然生效，同名注入没有 shadow。
    assert_eq!(recorder.calls.lock().unwrap().len(), 1);
    assert_eq!(*shadow_calls.lock().unwrap(), 0);
    // no-op 不产生新 header。
    assert_eq!(
        summary
            .log
            .events()
            .iter()
            .filter(|event| matches!(event, RecordEvent::Header(_)))
            .count(),
        1
    );
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].prompt.tools.len(), 1);
    assert_eq!(requests[1].prompt.tools[0].name().as_str(), "note");
}

#[tokio::test]
async fn a_missing_tool_remove_is_a_noop() {
    let recorder = Arc::new(Recorder::new());
    let llm = ScriptedLlm::new(vec![
        response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
        response(completed("done")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(RemoveToolOn::new(
            Trigger::Finished,
            ToolName::from_str("ghost").unwrap(),
        ))
        .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
        .build()
        .unwrap();
    let config = config_with(host.tools().specs(), 8);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    assert_eq!(recorder.calls.lock().unwrap().len(), 1);
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].prompt.tools.len(), 1);
    assert_eq!(
        summary
            .log
            .events()
            .iter()
            .filter(|event| matches!(event, RecordEvent::Header(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_command_emitted_after_the_turn_closes_is_dropped() {
    let llm = ScriptedLlm::new(vec![
        response(completed("done")),
        response(completed("again")),
    ]);
    let mut host = Host::builder(&llm, &NoInput)
        .model(model())
        .plugin(Aborter::new(Trigger::TurnEnd))
        .build()
        .unwrap();
    let config = config_with(host.tools().specs(), 4);
    let mut sink = Collector(Vec::new());
    let summary = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();

    assert_eq!(summary.outcome, Outcome::Answered);
    assert!(summary.aborted.is_none());
    assert!(
        !sink
            .0
            .iter()
            .any(|event| matches!(event, Event::Aborted { .. }))
    );

    // 迟到的中止不会泄漏到下一次 run。
    let config = config_with(host.tools().specs(), 4);
    let mut sink = Collector(Vec::new());
    let second = host
        .run(RunRequest::new(config, Log::default(), &mut sink))
        .await
        .unwrap();
    assert_eq!(second.outcome, Outcome::Answered);
    assert!(second.aborted.is_none());
}

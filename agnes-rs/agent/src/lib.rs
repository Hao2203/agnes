//! 驱动器：事件循环，解释 [`agnes_core::turn`] 状态机产生的动作。
//!
//! 机器只接收输入、消费事件、更新状态、产生待执行的动作；这个 crate 做
//! IO —— 通过 `agnes-io` 的异步接缝 —— 并把结果作为
//! [`agnes_core::turn::Input`] 喂回机器，一路用 [`Event`] 上报发生的事。
//! 运行时（tokio、smol、…）由宿主的执行器与传输层选择，与本 crate 无关。
//!
//! 驱动从不自己组装提示词：[`RequestBuilder`] 是宿主的转换者，
//! 在每次模型调用前由它把 Context 变成请求。

pub mod tools;

use std::sync::Arc;

use agnes_core::config::ModelName;
use agnes_core::content::Content;
use agnes_core::context::Context;
use agnes_core::llm::{LlmRequest, Prompt, RequestBuilder};
use agnes_core::message::{TokenUsage, ToolCall};
use agnes_core::tool::ToolName;
use agnes_core::turn::{Action, Input, Outcome, Turn};
use agnes_io::input::Input as InputPort;
use agnes_io::llm::{Llm, LlmError};
use agnes_io::tool::ToolRegistry;
use jiff::Timestamp;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] LlmError),
}

/// 标准转换者：Context 的标准投影（[`Prompt::from_context`]）加模型名。
/// 对提示词有想法的宿主自己实现 [`RequestBuilder`]。
#[derive(Debug, Clone)]
pub struct StandardRequestBuilder {
    model: ModelName,
}

impl StandardRequestBuilder {
    pub fn new(model: ModelName) -> Self {
        Self { model }
    }
}

impl RequestBuilder for StandardRequestBuilder {
    fn build(&self, context: &Context) -> LlmRequest {
        LlmRequest {
            model: self.model.clone(),
            prompt: Prompt::from_context(context),
        }
    }
}

/// 运行期间的上报：run 级生命周期事件，或 Context 日志上的一条事件。
#[derive(Debug, Clone)]
pub enum Event {
    Started {
        at: Timestamp,
        goal: Content,
        tools: Vec<ToolName>,
    },
    /// Agent 循环里发生的一条事件 —— turn/step 生命周期或消息，
    /// 按发生顺序转发。
    Recorded {
        at: Timestamp,
        event: agnes_core::event::Event,
    },
    Finished {
        at: Timestamp,
        outcome: Outcome,
    },
}

/// 接收 [`Event`]s。任何 `FnMut(&Event)` 都是个 `Sink`。
pub trait Sink {
    fn event(&mut self, event: &Event);
}

impl<F: FnMut(&Event)> Sink for F {
    fn event(&mut self, event: &Event) {
        self(event)
    }
}

/// 一次完整 turn 的结果。
///
/// `context` 是收场后的会话 —— 系统提示、环境、完整的有序日志；
/// 喂回下一个 [`agnes_core::turn::AgentConfig`] 即可延续会话。
/// `pending_user_messages` 是没来得及生效的插话（比如最终答复期间
/// 敲进来的），宿主接着用。
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub outcome: Outcome,
    pub steps_used: u32,
    pub usage: TokenUsage,
    pub context: Context,
    pub pending_user_messages: Vec<Content>,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
}

impl RunSummary {
    pub fn duration(&self) -> jiff::SignedDuration {
        jiff::SignedDuration::from_secs(self.finished_at.as_second() - self.started_at.as_second())
    }
}

/// 事件循环：把机器产生的动作执行掉，把 IO 的结果喂回机器。
///
/// 控制：中途打断以 [`Input::Interrupted`] 喂回 —— 模型响应与工具执行
/// 在两个等待阶段都随时可断，in-flight 的 future 就地丢弃。
/// 工具失败不中止 run —— 它是给模型的错误输出；只有传输/模型错误终止。
/// 用户运行中的插话由机器缓存、在下一个 step 边界生效；没生效的
/// 随 [`RunSummary::pending_user_messages`] 还给宿主。
pub async fn run(
    config: agnes_core::turn::AgentConfig,
    registry: Arc<ToolRegistry>,
    llm: &dyn Llm,
    input: &dyn InputPort,
    requests: &dyn RequestBuilder,
    sink: &mut dyn Sink,
) -> Result<RunSummary, AgentError> {
    let started_at = Timestamp::now();
    let goal = config.goal.clone();
    let tools = config
        .context
        .environment()
        .tools()
        .iter()
        .map(|spec| spec.name.clone())
        .collect();
    sink.event(&Event::Started {
        at: started_at,
        goal,
        tools,
    });

    let mut turn = Turn::start(config);
    let mut pending_model: Option<LlmRequest> = None;
    let mut pending_tool: Option<ToolCall> = None;
    interpret(
        &mut turn,
        requests,
        &mut pending_model,
        &mut pending_tool,
        sink,
    );

    while turn.outcome().is_none() {
        // 运行中的插话随时进缓存；生效时机由机器按 step 边界决定。
        for content in input.drain() {
            turn.handle(Input::UserMessage(content));
        }
        interpret(
            &mut turn,
            requests,
            &mut pending_model,
            &mut pending_tool,
            sink,
        );
        if turn.outcome().is_some() {
            break;
        }

        // turn 开着却没有待执行的 IO —— 只可能是驱动器状态坏了。
        match (pending_tool.take(), pending_model.take()) {
            (Some(call), _) => {
                turn.handle(Input::ToolStarted(call.id.clone()));
                interpret(
                    &mut turn,
                    requests,
                    &mut pending_model,
                    &mut pending_tool,
                    sink,
                );
                let raced = tokio::select! {
                    biased;
                    () = input.interrupted() => None,
                    output = registry.execute(&call) => Some(output),
                };
                turn.handle(match raced {
                    Some(output) => Input::ToolFinished {
                        call_id: call.id.clone(),
                        output,
                    },
                    None => Input::Interrupted,
                });
            }
            (None, Some(request)) => {
                let raced = tokio::select! {
                    biased;
                    () = input.interrupted() => None,
                    response = llm.complete(&request) => Some(response),
                };
                match raced {
                    None => turn.handle(Input::Interrupted),
                    Some(Ok(response)) => turn.handle(Input::ModelResponded(response)),
                    Some(Err(err)) => return Err(AgentError::Llm(err)),
                }
            }
            (None, None) => unreachable!("开着的 turn 一定有待执行的 IO"),
        }
    }

    // turn 收场时入队的报告事件在这里补报。
    interpret(
        &mut turn,
        requests,
        &mut pending_model,
        &mut pending_tool,
        sink,
    );

    let finished_at = Timestamp::now();
    let outcome = turn.outcome().expect("循环只在 turn 收场后退出").clone();
    let steps_used = turn.steps_used();
    let usage = turn.usage();
    let pending_user_messages = turn.take_unapplied_user_messages();
    let context = turn.into_context();
    let summary = RunSummary {
        outcome: outcome.clone(),
        steps_used,
        usage,
        context,
        pending_user_messages,
        started_at,
        finished_at,
    };
    sink.event(&Event::Finished {
        at: finished_at,
        outcome,
    });
    Ok(summary)
}

/// 执行一批动作：上报事件、构造模型请求、拾起下一个工具调用。
fn interpret(
    turn: &mut Turn,
    requests: &dyn RequestBuilder,
    pending_model: &mut Option<LlmRequest>,
    pending_tool: &mut Option<ToolCall>,
    sink: &mut dyn Sink,
) {
    while let Some(action) = turn.poll_actions() {
        match action {
            Action::Report(event) => sink.event(&Event::Recorded {
                at: Timestamp::now(),
                event,
            }),
            Action::CallModel => *pending_model = Some(requests.build(turn.context())),
            Action::RunTool(call) => *pending_tool = Some(call),
        }
    }
}

/// 宿主可用的默认 step 预算。
pub const DEFAULT_STEP_BUDGET: u32 = 32;

/// 便捷构造：把最大 step 数变成预算 newtype；0 返回 None。
pub fn step_budget(max_steps: u32) -> Option<agnes_core::config::StepBudget> {
    Some(agnes_core::config::StepBudget::new(
        std::num::NonZeroU32::new(max_steps)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::SystemPrompt;
    use agnes_core::context::Environment;
    use agnes_core::llm::{LlmResponse, StopReason};
    use agnes_core::message::{Message, ToolArguments, ToolOutput};
    use agnes_core::tool::{JsonSchema, ToolError, ToolSpec};
    use agnes_io::input::NoInput;
    use agnes_io::tool::Tool;
    use agnes_util::NonEmpty;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::num::NonZeroU32;
    use std::str::FromStr;
    use std::sync::Mutex;
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
                spec: ToolSpec {
                    name: ToolName::from_str("note").unwrap(),
                    description: "records a note".into(),
                    parameters: JsonSchema::try_from(serde_json::json!({
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }))
                    .unwrap(),
                },
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

    /// 收集所有事件的 Sink。
    struct Collector(Vec<Event>);

    impl Sink for Collector {
        fn event(&mut self, event: &Event) {
            self.0.push(event.clone());
        }
    }

    fn config_with(tools: Vec<ToolSpec>, budget: u32) -> agnes_core::turn::AgentConfig {
        agnes_core::turn::AgentConfig {
            context: Context::new(SystemPrompt::new("sys"), Environment::new(tools)),
            goal: Content::try_from("take a note").unwrap(),
            budget: agnes_core::config::StepBudget::new(NonZeroU32::new(budget).unwrap()),
        }
    }

    fn config(budget: u32) -> agnes_core::turn::AgentConfig {
        config_with(Vec::new(), budget)
    }

    fn model() -> ModelName {
        ModelName::from_str("test-model").unwrap()
    }

    fn standard(model: ModelName) -> StandardRequestBuilder {
        StandardRequestBuilder::new(model)
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

    fn tool_use(calls: Vec<agnes_core::message::ToolCall>) -> StopReason {
        StopReason::ToolUse {
            content: None,
            calls: NonEmpty::from_vec(calls).unwrap(),
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> agnes_core::message::ToolCall {
        agnes_core::message::ToolCall {
            id: agnes_core::message::ToolCallId::new(id),
            name: ToolName::from_str(name).unwrap(),
            arguments: ToolArguments::new(arguments),
        }
    }

    /// 驱动上报的运行时事件，按序。
    fn recorded(events: &[Event]) -> Vec<agnes_core::event::Event> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Recorded { event, .. } => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    /// 事件种类，按序。
    fn shape(events: &[agnes_core::event::Event]) -> Vec<&'static str> {
        use agnes_core::event::Event as CoreEvent;
        events
            .iter()
            .map(|event| match event {
                CoreEvent::TurnStart => "turn_start",
                CoreEvent::StepStart { .. } => "step_start",
                CoreEvent::MessageStart { .. } => "message_start",
                CoreEvent::MessageUpdate { .. } => "message_update",
                CoreEvent::Message(_) => "message",
                CoreEvent::MessageEnd { .. } => "message_end",
                CoreEvent::ToolExecutionStart { .. } => "tool_start",
                CoreEvent::ToolExecutionEnd { .. } => "tool_end",
                CoreEvent::StepEnd { .. } => "step_end",
                CoreEvent::UserMessagesInjected { .. } => "user_injected",
                CoreEvent::TurnEnd { .. } => "turn_end",
            })
            .collect()
    }

    fn messages_of(context: &Context) -> Vec<&Message> {
        context.messages().collect()
    }

    #[tokio::test]
    async fn happy_path_runs_tools_and_finishes() {
        let recorder = Arc::new(Recorder::new());
        let registry = ToolRegistry::build(vec![Arc::clone(&recorder) as Arc<dyn Tool>]).unwrap();
        let llm = ScriptedLlm::new(vec![
            response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
            response(completed("all done")),
        ]);

        let mut sink = Collector(Vec::new());
        let summary = run(
            config(8),
            Arc::new(registry),
            &llm,
            &NoInput,
            &standard(model()),
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(
            summary.outcome,
            Outcome::Answered {
                reply: Content::try_from("all done").unwrap()
            }
        );
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
        let messages = messages_of(&summary.context);
        assert_eq!(messages.len(), 4);
        assert!(matches!(messages[0], Message::User { .. }));
        assert!(matches!(messages[3], Message::Assistant(_)));
        assert_eq!(summary.context.system().as_str(), "sys");

        // 驱动按顺序上报了整个生命周期
        let events = &sink.0;
        assert!(matches!(events[0], Event::Started { .. }));
        assert_eq!(
            shape(&recorded(events)),
            vec![
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
                if matches!(outcome, Outcome::Answered { .. })
        ));
        // 上报的事件就是 Context 日志里的那些
        assert_eq!(recorded(events).len(), summary.context.events().len());
        assert!(summary.pending_user_messages.is_empty());
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
                Err(ToolError::Failed {
                    detail: "disk on fire".to_owned(),
                })
            }
        }

        let registry = ToolRegistry::build(vec![Arc::new(AlwaysFails {
            spec: ToolSpec {
                name: ToolName::from_str("boom").unwrap(),
                description: "".into(),
                parameters: JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
            },
        }) as Arc<dyn Tool>])
        .unwrap();
        let llm = ScriptedLlm::new(vec![
            response(tool_use(vec![call("c1", "boom", "{}")])),
            response(completed("gave up")),
        ]);

        let mut sink = Collector(Vec::new());
        let summary = run(
            config(8),
            Arc::new(registry),
            &llm,
            &NoInput,
            &standard(model()),
            &mut sink,
        )
        .await
        .unwrap();
        assert!(matches!(summary.outcome, Outcome::Answered { .. }));
        assert!(recorded(&sink.0).iter().any(|event| matches!(
            event,
            agnes_core::event::Event::ToolExecutionEnd { is_error: true, .. }
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
                    detail: "down".to_owned(),
                })
            }
        }
        let err = run(
            config(4),
            Arc::new(ToolRegistry::default()),
            &Broken,
            &NoInput,
            &standard(model()),
            &mut Collector(Vec::new()),
        )
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
            let mut prompt = Prompt::from_context(context);
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
        let registry =
            Arc::new(ToolRegistry::build(vec![Arc::clone(&recorder) as Arc<dyn Tool>]).unwrap());
        let llm = ScriptedLlm::new(vec![response(completed("ok"))]);

        let mut sink = Collector(Vec::new());
        let summary = run(
            config_with(registry.specs(), 4),
            Arc::clone(&registry),
            &llm,
            &NoInput,
            &PrimingRequests { model: model() },
            &mut sink,
        )
        .await
        .unwrap();
        assert!(matches!(summary.outcome, Outcome::Answered { .. }));

        let requests = llm.requests();
        assert_eq!(requests.len(), 1);
        // 标准投影本会带上 recorder；这个转换者把工具丢掉，改为垫话
        assert!(requests[0].prompt.tools.is_empty());
        assert!(matches!(
            &requests[0].prompt.input[0],
            Message::User { content } if content.as_str() == "<system> be extra brief"
        ));
        // 环境仍在会话里，未被破坏
        assert_eq!(summary.context.environment().tools().len(), 1);
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
    async fn queued_user_message_is_injected_at_the_step_boundary() {
        let recorder = Arc::new(Recorder::new());
        let registry = ToolRegistry::build(vec![Arc::clone(&recorder) as Arc<dyn Tool>]).unwrap();
        let llm = ScriptedLlm::new(vec![
            response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
            response(completed("all done")),
        ]);
        let input = FakeInput::new();
        input.queue("also update the readme");

        let mut sink = Collector(Vec::new());
        let summary = run(
            config(8),
            Arc::new(registry),
            &llm,
            &input,
            &standard(model()),
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(
            summary.outcome,
            Outcome::Answered {
                reply: Content::try_from("all done").unwrap()
            }
        );
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        let second = &requests[1].prompt.input;
        // system, goal, assistant (tool calls), tool result, injected user
        assert_eq!(second.len(), 5);
        assert!(matches!(&second[3], Message::ToolResult { .. }));
        assert!(matches!(
            &second[4],
            Message::User { content } if content.as_str() == "also update the readme"
        ));
        // 注入事件随日志上报
        assert!(recorded(&sink.0).iter().any(|event| matches!(
            event,
            agnes_core::event::Event::UserMessagesInjected { messages } if messages.len() == 1
        )));
        // 注入的消息属于继续的会话上下文
        let messages = messages_of(&summary.context);
        assert_eq!(messages.len(), 5);
        assert!(matches!(
            messages[3],
            Message::User { content } if content.as_str() == "also update the readme"
        ));
        assert!(summary.pending_user_messages.is_empty());
    }

    #[tokio::test]
    async fn interrupt_during_a_model_call_stops_the_run() {
        let input = FakeInput::new();
        let requests = standard(model());
        let mut sink = Collector(Vec::new());
        // 先让 run 到达挂起的模型调用，再打断
        let interrupter = async {
            tokio::task::yield_now().await;
            input.interrupt();
        };
        let (result, ()) = tokio::join!(
            run(
                config(4),
                Arc::new(ToolRegistry::default()),
                &HangingLlm,
                &input,
                &requests,
                &mut sink,
            ),
            interrupter
        );
        let summary = result.unwrap();

        assert_eq!(summary.outcome, Outcome::Interrupted);
        assert_eq!(summary.steps_used, 1);
        assert_eq!(summary.usage, TokenUsage::default());
        // 只剩 goal —— 被丢弃的响应从未入账
        assert_eq!(messages_of(&summary.context).len(), 1);
        assert!(matches!(
            messages_of(&summary.context)[0],
            Message::User { .. }
        ));
        // turn 被干净地收场，不是悬着
        assert_eq!(
            shape(summary.context.events()),
            vec![
                "message",
                "turn_start",
                "step_start",
                "message_start",
                "step_end",
                "turn_end"
            ]
        );
        assert!(matches!(
            summary.context.events().last(),
            Some(agnes_core::event::Event::TurnEnd {
                outcome: Outcome::Interrupted,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn interrupt_during_tools_settles_the_whole_batch() {
        let recorder = Arc::new(Recorder::new());
        let registry = ToolRegistry::build(vec![
            Arc::clone(&recorder) as Arc<dyn Tool>,
            Arc::new(HangingTool {
                spec: ToolSpec {
                    name: ToolName::from_str("hang").unwrap(),
                    description: "never returns".into(),
                    parameters: JsonSchema::try_from(serde_json::json!({"type": "object"}))
                        .unwrap(),
                },
            }) as Arc<dyn Tool>,
        ])
        .unwrap();
        let llm = ScriptedLlm::new(vec![response(tool_use(vec![
            call("c1", "note", r#"{"text":"hi"}"#),
            call("c2", "hang", "{}"),
        ]))]);
        let input = FakeInput::new();
        let requests = standard(model());
        let mut sink = Collector(Vec::new());
        // 第一个调用跑完、第二个挂起时打断
        let interrupter = async {
            tokio::task::yield_now().await;
            input.interrupt();
        };
        let (result, ()) = tokio::join!(
            run(
                config(8),
                Arc::new(registry),
                &llm,
                &input,
                &requests,
                &mut sink,
            ),
            interrupter
        );
        let summary = result.unwrap();

        assert_eq!(summary.outcome, Outcome::Interrupted);
        assert_eq!(summary.steps_used, 1);
        assert_eq!(llm.requests().len(), 1); // 没有后续模型调用
        assert_eq!(recorder.calls.lock().unwrap().len(), 1);
        let messages = messages_of(&summary.context);
        // goal, assistant (tool calls), tool result, interrupted tool result
        assert_eq!(messages.len(), 4);
        assert!(matches!(
            messages[2],
            Message::ToolResult { output, .. } if !output.is_error
        ));
        assert!(matches!(
            messages[3],
            Message::ToolResult { output, .. }
                if output.is_error && output.content.contains("interrupted")
        ));
    }

    #[tokio::test]
    async fn unapplied_user_messages_come_back_to_the_host() {
        let input = FakeInput::new();
        // 最终答复期间敲进来的插话：边界未到，未生效
        input.queue("too late for this run");
        let llm = ScriptedLlm::new(vec![response(completed("all done"))]);

        let mut sink = Collector(Vec::new());
        let summary = run(
            config(4),
            Arc::new(ToolRegistry::default()),
            &llm,
            &input,
            &standard(model()),
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(
            summary.outcome,
            Outcome::Answered {
                reply: Content::try_from("all done").unwrap()
            }
        );
        // 插话没有进会话；会话里只有 goal 和最终答复
        assert_eq!(
            summary.pending_user_messages,
            vec![Content::try_from("too late for this run").unwrap()]
        );
        assert_eq!(messages_of(&summary.context).len(), 2);
    }
}

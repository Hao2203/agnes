//! 驱动器：宿主状态机（私有模块 `driver`）解释 [`agnes_core::turn`] 状态机
//! 产生的动作 —— 就地处理动作、管理等待点与输入输出。
//!
//! 机器是类型状态的（阶段即类型）；这个 crate 做 IO —— 通过 `agnes-io`
//! 的异步接缝 —— 把结果经对应阶段的方法喂回机器，一路用 [`Event`] 上报
//! 发生的事。运行时（tokio、smol、…）由宿主的执行器与传输层选择，与本
//! crate 无关。
//!
//! 宿主用 [`Host`] 装配：模型、输入、工具、扩展内核、审批口与会话日志在
//! 装载期一次性解析。驱动从不自己组装提示词：[`RequestBuilder`] 是宿主
//! 的转换者，在每次模型调用前由它把日志变成请求。
//!
//! 工具执行前先过扩展内核（[`ext`]）：插件用类型化钩子表态（门禁 /
//! 结果 / 观察），执行权在系统 —— 放行执行、拒绝则以合成失败结果推进
//! 机器（core 日志只是个普通失败工具）、要审批则走系统自有的审批服务
//! （[`approval`]）挂起等待答覆或打断。core 的封闭枚举不因插件扩展 ——
//! 插件的语义都发在宿主的 [`Event`] 变体上。

mod driver;
mod host;

pub mod approval;
pub mod ext;
pub mod permission;
pub mod tools;

pub use host::{Host, HostBuilder, LoadError};

use agnes_core::config::ModelName;
use agnes_core::content::Content;
use agnes_core::event::Event as CoreEvent;
use agnes_core::llm::{LlmRequest, Prompt, RequestBuilder};
use agnes_core::log::Log;
use agnes_core::message::{TokenUsage, ToolCall, ToolCallId};
use agnes_core::tool::ToolName;
use agnes_core::turn::Outcome;
use agnes_io::llm::LlmError;
use jiff::Timestamp;

use approval::ApprovalOutcome;
use ext::Denied;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    /// 会话日志写失败：宁可中止，也不在没有审计轨迹的情况下继续跑。
    #[error(transparent)]
    Session(#[from] agnes_io::session::SessionError),
}

/// 标准转换者：日志的标准投影（[`Prompt::from_log`]）加模型名。
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
    fn build(&self, log: &Log) -> LlmRequest {
        LlmRequest {
            model: self.model.clone(),
            prompt: Prompt::from_log(log),
        }
    }
}

/// 运行期间的上报：run 级生命周期事件、日志的镜像事件，以及
/// 插件语义的宿主事件 —— 插件的面貌只长在这里，core 不见。
#[derive(Debug, Clone)]
pub enum Event {
    Started {
        at: Timestamp,
        goal: Content,
        tools: Vec<ToolName>,
    },
    /// Agent 循环里发生的一条核心事件 —— turn/step 生命周期或消息，
    /// 按发生顺序转发。
    Recorded {
        at: Timestamp,
        event: CoreEvent,
    },
    /// 扩展内核拒绝了一次工具调用；机器将收到一条合成的失败结果。
    ToolGated {
        at: Timestamp,
        call: ToolCall,
        /// 拒绝者与理由（理由随合成结果一起给到模型）。
        denied: Denied,
    },
    /// 权限插件发起的审批问询；驱动随即挂起，等待答覆或打断。
    ApprovalRequested {
        at: Timestamp,
        call: ToolCall,
    },
    /// 一问一答落定：只有 [`ApprovalOutcome::AllowedOnce`] 照常执行，其余
    /// （显式拒绝、超时、答覆消失、打断）都以合成失败结果推进。
    ApprovalResolved {
        at: Timestamp,
        call_id: ToolCallId,
        outcome: ApprovalOutcome,
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
/// `log` 是收场后的会话日志（系统提示与工具环境也在其中，作为事件）；
/// 喂回下一个 [`agnes_core::turn::AgentConfig`] 的 `log` 即可延续会话。
/// `pending_user_messages` 是没来得及生效的插话（预算耗尽、最后一次答复
/// 时仍没轮到），宿主接着用。
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub outcome: Outcome,
    pub steps_used: u32,
    pub usage: TokenUsage,
    pub log: Log,
    pub pending_user_messages: Vec<Content>,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
}

impl RunSummary {
    pub fn duration(&self) -> jiff::SignedDuration {
        jiff::SignedDuration::from_secs(self.finished_at.as_second() - self.started_at.as_second())
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
    use crate::approval::ApprovalChannel;
    use crate::ext::{ApprovalOutcome, Notice, Plugin, Settlement};
    use crate::permission::{PermissionPlugin, Rule};
    use agnes_core::config::{StepBudget, SystemPrompt};
    use agnes_core::header::{Environment, Header};
    use agnes_core::llm::{LlmResponse, StopReason};
    use agnes_core::message::{Message, ToolArguments, ToolOutput};
    use agnes_core::tool::{JsonSchema, ToolError, ToolSpec};
    use agnes_io::input::{Input as InputPort, NoInput};
    use agnes_io::llm::Llm;
    use agnes_io::session::HostEvent;
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
            log: Log::default(),
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
            model: model(),
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
    fn recorded(events: &[Event]) -> Vec<CoreEvent> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Recorded { event, .. } => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    /// 事件种类，按序。
    fn shape(events: &[CoreEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                CoreEvent::Header(_) => "header",
                CoreEvent::TurnStart => "turn_start",
                CoreEvent::StepStart { .. } => "step_start",
                CoreEvent::MessageStart { .. } => "message_start",
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
        let summary = host.run(config(8), &mut sink, None).await.unwrap();

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
        let messages = messages_of(&summary.log);
        assert_eq!(messages.len(), 4);
        assert!(matches!(messages[0], Message::User { .. }));
        assert!(matches!(messages[3], Message::Assistant(_)));
        assert_eq!(summary.log.header().unwrap().system.as_str(), "sys");

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
                if matches!(outcome, Outcome::Answered { .. })
        ));
        // 上报的核心事件就是日志里的那些
        assert_eq!(recorded(events).len(), summary.log.events().len());
        assert!(summary.pending_user_messages.is_empty());
    }

    /// 记录落盘事件、审计记录与 flush 次数的会话日志。
    struct RecordingSession {
        events: Arc<Mutex<Vec<CoreEvent>>>,
        notes: Arc<Mutex<Vec<HostEvent>>>,
        flushes: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl agnes_io::session::SessionLog for RecordingSession {
        async fn append(
            &mut self,
            _at: jiff::Timestamp,
            event: &CoreEvent,
        ) -> Result<(), agnes_io::session::SessionError> {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }

        async fn append_host(
            &mut self,
            _at: jiff::Timestamp,
            event: &HostEvent,
        ) -> Result<(), agnes_io::session::SessionError> {
            self.notes.lock().unwrap().push(event.clone());
            Ok(())
        }

        async fn flush(&mut self) -> Result<(), agnes_io::session::SessionError> {
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
            .run(config(8), &mut sink, Some(&mut session))
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
            .plugin(PermissionPlugin::new(
                vec![],
                vec![Rule::Name(ToolName::from_str("note").unwrap())],
            ))
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
        host.run(config(8), &mut sink, Some(&mut session))
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
                    detail: "down".to_owned(),
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
            .run(config(4), &mut Collector(Vec::new()), Some(&mut session))
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
                Err(ToolError::Failed {
                    detail: "disk on fire".to_owned(),
                })
            }
        }

        let tool = Arc::new(AlwaysFails {
            spec: ToolSpec {
                name: ToolName::from_str("boom").unwrap(),
                description: "".into(),
                parameters: JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
            },
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
        let summary = host.run(config(8), &mut sink, None).await.unwrap();
        assert!(matches!(summary.outcome, Outcome::Answered { .. }));
        assert!(
            recorded(&sink.0)
                .iter()
                .any(|event| matches!(event, CoreEvent::ToolExecutionEnd { is_error: true, .. }))
        );
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
        let llm = Broken;
        let mut host = Host::builder(&llm, &NoInput)
            .model(model())
            .build()
            .unwrap();
        let err = host
            .run(config(4), &mut Collector(Vec::new()), None)
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
        fn build(&self, log: &Log) -> LlmRequest {
            let mut prompt = Prompt::from_log(log);
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
        let summary = host.run(config, &mut sink, None).await.unwrap();
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
        assert_eq!(summary.log.header().unwrap().environment.tools().len(), 1);
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
        let summary = host.run(config(8), &mut sink, None).await.unwrap();

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
            CoreEvent::UserMessagesInjected { messages } if messages.len() == 1
        )));
        // 注入的消息属于继续的会话上下文
        let messages = messages_of(&summary.log);
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
        let llm = HangingLlm;
        let mut host = Host::builder(&llm, &input).model(model()).build().unwrap();
        let mut sink = Collector(Vec::new());
        // 先让 run 到达挂起的模型调用，再打断
        let interrupter = async {
            tokio::task::yield_now().await;
            input.interrupt();
        };
        let (result, ()) = tokio::join!(host.run(config(4), &mut sink, None), interrupter);
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
            Some(CoreEvent::TurnEnd {
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
                spec: ToolSpec {
                    name: ToolName::from_str("hang").unwrap(),
                    description: "never returns".into(),
                    parameters: JsonSchema::try_from(serde_json::json!({"type": "object"}))
                        .unwrap(),
                },
            }))
            .build()
            .unwrap();
        let mut sink = Collector(Vec::new());
        // 第一个调用跑完、第二个挂起时打断
        let interrupter = async {
            tokio::task::yield_now().await;
            input.interrupt();
        };
        let (result, ()) = tokio::join!(host.run(config(8), &mut sink, None), interrupter);
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
            Message::ToolResult { output, .. } if !output.is_error
        ));
        assert!(matches!(
            messages[3],
            Message::ToolResult { output, .. }
                if output.is_error && output.content.contains("interrupted")
        ));
    }

    #[tokio::test]
    async fn a_queued_message_continues_the_turn_after_an_answer() {
        let input = FakeInput::new();
        // 模型还在生成答复时敲进来的插话：收件箱非空 → 同一 turn 续跑
        input.queue("also update the readme");
        let llm = ScriptedLlm::new(vec![
            response(completed("first answer")),
            response(completed("second answer")),
        ]);
        let mut host = Host::builder(&llm, &input).model(model()).build().unwrap();

        let mut sink = Collector(Vec::new());
        let summary = host.run(config(4), &mut sink, None).await.unwrap();

        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        // 第二次请求看得见这条插话
        assert!(requests[1].prompt.input.iter().any(
            |message| matches!(message, Message::User { content } if content.as_str() == "also update the readme")
        ));
        assert_eq!(
            summary.outcome,
            Outcome::Answered {
                reply: Content::try_from("second answer").unwrap()
            }
        );
        assert_eq!(summary.steps_used, 2);
        assert!(summary.pending_user_messages.is_empty());
        assert!(recorded(&sink.0).iter().any(|event| matches!(
            event,
            CoreEvent::UserMessagesInjected { messages } if messages.len() == 1
        )));
    }

    #[tokio::test]
    async fn a_queued_message_comes_back_when_the_budget_blocks_another_step() {
        let input = FakeInput::new();
        // 一步预算：答复后没有余量续跑，插话还给宿主
        input.queue("too late for this run");
        let llm = ScriptedLlm::new(vec![response(completed("all done"))]);
        let mut host = Host::builder(&llm, &input).model(model()).build().unwrap();

        let mut sink = Collector(Vec::new());
        let summary = host.run(config(1), &mut sink, None).await.unwrap();

        assert_eq!(
            summary.outcome,
            Outcome::Answered {
                reply: Content::try_from("all done").unwrap()
            }
        );
        // 插话没有进会话；会话里只有 goal 和答复
        assert_eq!(
            summary.pending_user_messages,
            vec![Content::try_from("too late for this run").unwrap()]
        );
        assert_eq!(messages_of(&summary.log).len(), 2);
    }

    // —— 插件：权限链驱动宿主 FSM ——

    /// 记录观察到的通知、按配置折叠结果。
    struct Witness {
        settlement: Settlement,
        seen: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Witness {
        fn new(settlement: Settlement, seen: &Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                settlement,
                seen: Arc::clone(seen),
            }
        }
    }

    impl Plugin for Witness {
        fn name(&self) -> &'static str {
            "witness"
        }

        fn settle(&mut self, _call: &ToolCall, _output: &ToolOutput) -> Settlement {
            self.settlement.clone()
        }

        fn observe(&mut self, notice: &Notice<'_>) {
            let label = match notice {
                Notice::Core(_) => return,
                Notice::Gated { .. } => "gated",
                Notice::Asked { .. } => "asked",
                Notice::Decided { .. } => "decided",
                Notice::Ran { .. } => "ran",
                Notice::Blocked { .. } => "blocked",
            };
            self.seen.lock().unwrap().push(label);
        }
    }

    #[tokio::test]
    async fn an_accepted_result_is_observed_as_ran() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::new(Recorder::new());
        let llm = ScriptedLlm::new(vec![
            response(tool_use(vec![call("c1", "note", r#"{"text":"hi"}"#)])),
            response(completed("ok")),
        ]);
        let mut host = Host::builder(&llm, &NoInput)
            .model(model())
            .plugin(Witness::new(Settlement::Accept, &seen))
            .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
            .build()
            .unwrap();

        let mut sink = Collector(Vec::new());
        let summary = host.run(config(8), &mut sink, None).await.unwrap();

        assert!(
            matches!(&messages_of(&summary.log)[2], Message::ToolResult { output, .. } if !output.is_error)
        );
        assert_eq!(*seen.lock().unwrap(), vec!["ran"]);
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
            .plugin(Witness::new(
                Settlement::Block {
                    feedback: "the note was rejected".into(),
                },
                &seen,
            ))
            .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
            .build()
            .unwrap();

        let mut sink = Collector(Vec::new());
        let summary = host.run(config(8), &mut sink, None).await.unwrap();

        // 工具确实跑过；入账的是反馈，仍然是那一个调用的一条结果
        assert_eq!(recorder.calls.lock().unwrap().len(), 1);
        assert!(
            matches!(&messages_of(&summary.log)[2], Message::ToolResult { output, .. }
                if output.is_error && output.content.contains("the note was rejected"))
        );
        assert!(
            recorded(&sink.0)
                .iter()
                .any(|event| matches!(event, CoreEvent::ToolExecutionEnd { is_error: true, .. }))
        );
        assert_eq!(*seen.lock().unwrap(), vec!["blocked"]);
        assert!(matches!(summary.outcome, Outcome::Answered { .. }));
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
            .plugin(PermissionPlugin::new(
                vec![Rule::Name(ToolName::from_str("note").unwrap())],
                vec![],
            ))
            .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
            .build()
            .unwrap();

        let mut sink = Collector(Vec::new());
        let summary = host.run(config(8), &mut sink, None).await.unwrap();

        // 工具没跑
        assert!(recorder.calls.lock().unwrap().is_empty());
        // core 日志里是一次普通的失败工具
        let messages = messages_of(&summary.log);
        assert_eq!(messages.len(), 4);
        assert!(matches!(&messages[2], Message::ToolResult { output, .. }
                if output.is_error && output.content.contains("denied by policy")));
        // 宿主上报了 gating
        assert!(
            sink.0
                .iter()
                .any(|event| matches!(event, Event::ToolGated { denied, .. }
                if denied.by == "permission" && denied.reason.contains("denied by policy")))
        );
        assert!(matches!(summary.outcome, Outcome::Answered { .. }));
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
            .plugin(PermissionPlugin::new(
                vec![],
                vec![Rule::Name(ToolName::from_str("note").unwrap())],
            ))
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
        let summary = host.run(config(8), &mut sink, None).await.unwrap();
        reviewer.await.unwrap();

        // 批准后工具真正运行了
        assert_eq!(
            recorder.calls.lock().unwrap().clone(),
            vec![r#"{"text":"hi"}"#.to_owned()]
        );
        assert!(sink.0.iter().any(|event|
            matches!(event, Event::ApprovalRequested { call, .. } if call.id.as_str() == "c1")
        ));
        assert!(sink.0.iter().any(|event|
            matches!(event, Event::ApprovalResolved { call_id, outcome: ApprovalOutcome::AllowedOnce, .. }
                if call_id.as_str() == "c1")
        ));
        assert!(matches!(summary.outcome, Outcome::Answered { .. }));
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
            .plugin(PermissionPlugin::new(
                vec![],
                vec![Rule::Name(ToolName::from_str("note").unwrap())],
            ))
            .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
            .approvals(Some(&approval))
            .build()
            .unwrap();
        let reviewer = tokio::spawn(async move {
            let question = questions.recv().await.expect("the question arrives");
            question.reply.send(false).expect("the refusal is sent");
        });

        let mut sink = Collector(Vec::new());
        let summary = host.run(config(8), &mut sink, None).await.unwrap();
        reviewer.await.unwrap();

        assert!(recorder.calls.lock().unwrap().is_empty());
        let messages = messages_of(&summary.log);
        assert!(matches!(&messages[2], Message::ToolResult { output, .. }
                if output.is_error && output.content.contains("denied by approval: the approval was refused")));
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
        assert!(matches!(summary.outcome, Outcome::Answered { .. }));
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
            .plugin(PermissionPlugin::new(
                vec![],
                vec![Rule::Name(ToolName::from_str("note").unwrap())],
            ))
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
        let (result, ()) = tokio::join!(host.run(config(8), &mut sink, None), interrupter);
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
                if output.is_error && output.content.contains("interrupted")));
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
            .plugin(PermissionPlugin::new(
                vec![],
                vec![Rule::Name(ToolName::from_str("note").unwrap())],
            ))
            .tool(Arc::clone(&recorder) as Arc<dyn Tool>)
            .approvals(Some(&approval))
            .build()
            .unwrap();
        let mut sink = Collector(Vec::new());
        let summary = host.run(config(8), &mut sink, None).await.unwrap();
        drop(kept_questions);

        assert!(matches!(summary.outcome, Outcome::Answered { .. }));
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
}

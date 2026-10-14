//! Async driver that interprets the [`agnes_core::turn`] effects.
//!
//! The state machine says *what* to do (call the model, run tools); this
//! crate does it — through the async ports in `agnes-ports` — and reports
//! [`Event`]s along the way. The runtime itself (tokio, smol, …) is chosen
//! by the host's executor and transport, never by this crate.

pub mod tools;

use std::sync::Arc;

use agnes_core::config::{ModelName, SystemPrompt, TurnBudget};
use agnes_core::content::Content;
use agnes_core::context::Context;
use agnes_core::llm::{LlmRequest, LlmResponse};
use agnes_core::message::{TokenUsage, ToolCall, ToolCallId, ToolOutput};
use agnes_core::tool::ToolName;
use agnes_core::turn::{Agent, AgentConfig, ModelTurn, Outcome, ToolBatch, ToolsTurn, TurnError};
use agnes_ports::input::Input;
use agnes_ports::llm::{Llm, LlmError};
use agnes_ports::tool::ToolRegistry;
use jiff::Timestamp;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error(transparent)]
    Turn(#[from] TurnError),
}

/// Progress notifications emitted while a run executes.
#[derive(Debug, Clone)]
pub enum Event {
    Started {
        at: Timestamp,
        goal: Content,
        model: ModelName,
        tools: Vec<ToolName>,
    },
    ModelRequested {
        at: Timestamp,
        turn: u32,
    },
    ModelResponded {
        at: Timestamp,
        stop: agnes_core::llm::StopReason,
        usage: TokenUsage,
    },
    ToolStarted {
        at: Timestamp,
        call: ToolCall,
    },
    ToolFinished {
        at: Timestamp,
        call_id: ToolCallId,
        is_error: bool,
    },
    /// User messages typed while the run was working, injected into the
    /// transcript after the tool results.
    UserMessagesInjected {
        at: Timestamp,
        messages: Vec<Content>,
    },
    Finished {
        at: Timestamp,
        outcome: Outcome,
    },
}

/// Receives [`Event`]s. Any `FnMut(&Event)` is a `Sink`.
pub trait Sink {
    fn event(&mut self, event: &Event);
}

impl<F: FnMut(&Event)> Sink for F {
    fn event(&mut self, event: &Event) {
        self(event)
    }
}

/// Result of a completed run. `context` is the updated session context —
/// system prompt plus every turn of the run (goals, replies, tool calls and
/// results). Feed it back into the next task's [`AgentConfig::context`] to
/// continue a session.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub outcome: Outcome,
    pub turns_used: u32,
    pub usage: TokenUsage,
    pub context: Context,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
}

impl RunSummary {
    pub fn duration(&self) -> jiff::SignedDuration {
        jiff::SignedDuration::from_secs(self.finished_at.as_second() - self.started_at.as_second())
    }
}

/// What one race between a model call and the user's stop request yielded.
enum ModelStep {
    /// The user interrupted; the in-flight request was abandoned.
    Interrupted,
    Responded(LlmResponse),
}

/// Drives the agent loop to completion: model call → (tools → model call)*
/// → outcome. Tool failures never abort the run — they are surfaced to the
/// model as error outputs; only transport/model errors abort. Messages the
/// user types while the run works are injected after the next tool batch;
/// an interruption ends the run with [`Outcome::Interrupted`].
///
/// The machine hands out prompts; this driver owns the model wiring, turning
/// each prompt into a concrete [`LlmRequest`].
pub async fn run(
    config: AgentConfig,
    model: ModelName,
    registry: Arc<ToolRegistry>,
    llm: &dyn Llm,
    input: &dyn Input,
    sink: &mut dyn Sink,
) -> Result<RunSummary, AgentError> {
    let started_at = Timestamp::now();
    let system = config.context.system().clone();
    let goal = config.goal.clone();
    let tools = config.tools.iter().map(|spec| spec.name.clone()).collect();
    sink.event(&Event::Started {
        at: started_at,
        goal,
        model: model.clone(),
        tools,
    });

    let (agent, prompt) = Agent::new(config).start();
    let mut agent = agent;
    let mut request = LlmRequest {
        model: model.clone(),
        prompt,
    };
    loop {
        sink.event(&Event::ModelRequested {
            at: Timestamp::now(),
            turn: agent.turns_used(),
        });
        let step = tokio::select! {
            biased;
            () = input.interrupted() => ModelStep::Interrupted,
            response = llm.complete(&request) => ModelStep::Responded(response?),
        };
        let response = match step {
            ModelStep::Interrupted => return finish(agent.interrupted(), system, started_at, sink),
            ModelStep::Responded(response) => response,
        };
        sink.event(&Event::ModelResponded {
            at: Timestamp::now(),
            stop: response.stop_reason.clone(),
            usage: response.usage,
        });
        match agent.model_responded(response)? {
            ModelTurn::NeedTools {
                agent: waiting,
                batch,
            } => {
                let (outputs, interrupted) = execute_batch(&registry, &batch, input, sink).await;
                let results = batch.complete(outputs)?;
                if interrupted {
                    return finish(waiting.interrupted(results), system, started_at, sink);
                }
                let user_messages = input.drain();
                if !user_messages.is_empty() {
                    sink.event(&Event::UserMessagesInjected {
                        at: Timestamp::now(),
                        messages: user_messages.clone(),
                    });
                }
                match waiting.tools_resolved(results, user_messages)? {
                    ToolsTurn::NeedModel {
                        agent: next,
                        prompt,
                    } => {
                        agent = next;
                        request = LlmRequest {
                            model: model.clone(),
                            prompt,
                        };
                    }
                    ToolsTurn::Done(finished) => return finish(finished, system, started_at, sink),
                }
            }
            ModelTurn::Done(finished) => return finish(finished, system, started_at, sink),
        }
    }
}

/// Runs the batch call by call, racing every execution against the user's
/// stop request. On interruption the call in flight is dropped (its result
/// is lost) and it — together with every call after it — is recorded as a
/// synthetic error output, so the batch still pairs one result per call.
async fn execute_batch(
    registry: &ToolRegistry,
    batch: &ToolBatch,
    input: &dyn Input,
    sink: &mut dyn Sink,
) -> (Vec<ToolOutput>, bool) {
    let mut outputs = Vec::with_capacity(batch.calls().len());
    let mut interrupted = false;
    for call in batch.calls() {
        let output = if interrupted {
            interrupted_output()
        } else {
            sink.event(&Event::ToolStarted {
                at: Timestamp::now(),
                call: call.clone(),
            });
            tokio::select! {
                biased;
                () = input.interrupted() => {
                    interrupted = true;
                    interrupted_output()
                }
                output = registry.execute(call) => output,
            }
        };
        sink.event(&Event::ToolFinished {
            at: Timestamp::now(),
            call_id: call.id.clone(),
            is_error: output.is_error,
        });
        outputs.push(output);
    }
    (outputs, interrupted)
}

/// Output recorded for a call that never ran — or whose run was cut off —
/// because the user interrupted the agent.
fn interrupted_output() -> ToolOutput {
    ToolOutput::error("not run: the user interrupted the agent")
}

fn finish(
    finished: Agent<agnes_core::turn::Finished>,
    system: SystemPrompt,
    started_at: Timestamp,
    sink: &mut dyn Sink,
) -> Result<RunSummary, AgentError> {
    let finished_at = Timestamp::now();
    let summary = RunSummary {
        outcome: finished.outcome().clone(),
        turns_used: finished.turns_used(),
        usage: finished.usage(),
        context: Context::from_parts(system, finished.into_transcript().into_history()),
        started_at,
        finished_at,
    };
    sink.event(&Event::Finished {
        at: finished_at,
        outcome: summary.outcome.clone(),
    });
    Ok(summary)
}

/// Default cap on model turns per task; hosts may pass their own.
pub const DEFAULT_TURN_BUDGET: u32 = 32;

/// Convenience constructor used by hosts: builds the turn budget newtype.
pub fn turn_budget(max_turns: u32) -> Option<TurnBudget> {
    Some(TurnBudget::new(std::num::NonZeroU32::new(max_turns)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::SystemPrompt;
    use agnes_core::llm::{LlmRequest, LlmResponse, StopReason};
    use agnes_core::message::ToolArguments;
    use agnes_core::tool::{JsonSchema, ToolError, ToolSpec};
    use agnes_ports::input::{Input, NoInput};
    use agnes_ports::tool::Tool;
    use agnes_util::NonEmpty;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::num::NonZeroU32;
    use std::str::FromStr;
    use std::sync::Mutex;
    use tokio::sync::watch;

    /// Model that replays a fixed script of responses and records every
    /// request it sees.
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

    /// Tool that records its arguments and echoes them back.
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

    /// Sink that collects every event for assertions.
    struct Collector(Vec<Event>);

    impl Sink for Collector {
        fn event(&mut self, event: &Event) {
            self.0.push(event.clone());
        }
    }

    fn config(budget: u32) -> AgentConfig {
        AgentConfig {
            context: Context::new(SystemPrompt::new("sys")),
            goal: Content::try_from("take a note").unwrap(),
            tools: Vec::new(),
            budget: TurnBudget::new(NonZeroU32::new(budget).unwrap()),
        }
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
            id: ToolCallId::new(id),
            name: ToolName::from_str(name).unwrap(),
            arguments: ToolArguments::new(arguments),
        }
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
            model(),
            Arc::new(registry),
            &llm,
            &NoInput,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(
            summary.outcome,
            Outcome::Completed {
                reply: Content::try_from("all done").unwrap()
            }
        );
        assert_eq!(summary.turns_used, 2);
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
        // user goal, assistant (tool call), tool result, assistant (final)
        assert_eq!(summary.context.len(), 4);
        assert!(matches!(
            summary.context.messages()[0],
            agnes_core::message::Message::User { .. }
        ));
        assert!(matches!(
            summary.context.messages()[3],
            agnes_core::message::Message::Assistant(_)
        ));
        assert_eq!(summary.context.system().as_str(), "sys");

        let events = &sink.0;
        assert!(matches!(events[0], Event::Started { .. }));
        assert!(matches!(events[1], Event::ModelRequested { turn: 1, .. }));
        assert!(matches!(
            events[2],
            Event::ModelResponded {
                stop: StopReason::ToolUse { .. },
                ..
            }
        ));
        assert!(matches!(&events[3], Event::ToolStarted { call, .. } if call.id.as_str() == "c1"));
        assert!(matches!(
            events[4],
            Event::ToolFinished {
                is_error: false,
                ..
            }
        ));
        assert!(matches!(events[5], Event::ModelRequested { turn: 2, .. }));
        assert!(matches!(
            events[6],
            Event::ModelResponded {
                stop: StopReason::Completed { .. },
                ..
            }
        ));
        assert!(matches!(
            &events[7],
            Event::Finished { outcome, .. } if matches!(outcome, Outcome::Completed { .. })
        ));
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
            model(),
            Arc::new(registry),
            &llm,
            &NoInput,
            &mut sink,
        )
        .await
        .unwrap();
        assert!(matches!(summary.outcome, Outcome::Completed { .. }));
        assert!(matches!(
            sink.0[4],
            Event::ToolFinished { is_error: true, .. }
        ));
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
            model(),
            Arc::new(ToolRegistry::default()),
            &Broken,
            &NoInput,
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

    /// Input the test drives directly: a message queue plus a stop channel.
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
    impl Input for FakeInput {
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

    /// Model whose call never comes back; the future is dropped when the
    /// user interrupts.
    struct HangingLlm;

    #[async_trait]
    impl Llm for HangingLlm {
        async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse, LlmError> {
            std::future::pending().await
        }
    }

    /// Tool whose run never comes back.
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
    async fn queued_user_message_is_injected_after_tool_results() {
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
            model(),
            Arc::new(registry),
            &llm,
            &input,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(
            summary.outcome,
            Outcome::Completed {
                reply: Content::try_from("all done").unwrap()
            }
        );
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        let second = &requests[1].prompt.input;
        // system, goal, assistant (tool calls), tool result, injected user
        assert_eq!(second.len(), 5);
        assert!(matches!(
            &second[3],
            agnes_core::message::Message::ToolResult { .. }
        ));
        assert!(matches!(
            &second[4],
            agnes_core::message::Message::User { content }
                if content.as_str() == "also update the readme"
        ));
        assert!(sink.0.iter().any(|event| matches!(
            event,
            Event::UserMessagesInjected { messages, .. } if messages.len() == 1
        )));
        // the injected turn is part of the continuing session context
        let messages = summary.context.messages();
        assert_eq!(messages.len(), 5);
        assert!(matches!(
            &messages[3],
            agnes_core::message::Message::User { content }
                if content.as_str() == "also update the readme"
        ));
    }

    #[tokio::test]
    async fn interrupt_during_a_model_call_stops_the_run() {
        let input = FakeInput::new();
        let mut sink = Collector(Vec::new());
        // let the run reach the pending model call first, then interrupt
        let interrupter = async {
            tokio::task::yield_now().await;
            input.interrupt();
        };
        let (result, ()) = tokio::join!(
            run(
                config(4),
                model(),
                Arc::new(ToolRegistry::default()),
                &HangingLlm,
                &input,
                &mut sink,
            ),
            interrupter
        );
        let summary = result.unwrap();

        assert_eq!(summary.outcome, Outcome::Interrupted);
        assert_eq!(summary.turns_used, 1);
        assert_eq!(summary.usage, TokenUsage::default());
        // just the goal — the abandoned response was never recorded
        assert_eq!(summary.context.len(), 1);
        assert!(matches!(
            &summary.context.messages()[0],
            agnes_core::message::Message::User { .. }
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
        let mut sink = Collector(Vec::new());
        // let the first call run and the second start hanging, then interrupt
        let interrupter = async {
            tokio::task::yield_now().await;
            input.interrupt();
        };
        let (result, ()) = tokio::join!(
            run(
                config(8),
                model(),
                Arc::new(registry),
                &llm,
                &input,
                &mut sink
            ),
            interrupter
        );
        let summary = result.unwrap();

        assert_eq!(summary.outcome, Outcome::Interrupted);
        assert_eq!(summary.turns_used, 1);
        assert_eq!(llm.requests().len(), 1); // no follow-up model call
        assert_eq!(recorder.calls.lock().unwrap().len(), 1);
        let messages = summary.context.messages();
        // goal, assistant (tool calls), tool result, interrupted tool result
        assert_eq!(messages.len(), 4);
        assert!(matches!(
            &messages[2],
            agnes_core::message::Message::ToolResult { output, .. } if !output.is_error
        ));
        assert!(matches!(
            &messages[3],
            agnes_core::message::Message::ToolResult { output, .. }
                if output.is_error && output.content.contains("interrupted")
        ));
    }
}

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
use agnes_core::message::{TokenUsage, ToolCall, ToolCallId, ToolOutput};
use agnes_core::tool::ToolName;
use agnes_core::turn::{Agent, AgentConfig, ModelTurn, Outcome, ToolBatch, ToolsTurn, TurnError};
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

/// Drives the agent loop to completion: model call → (tools → model call)*
/// → outcome. Tool failures never abort the run — they are surfaced to the
/// model as error outputs; only transport/model errors abort.
pub async fn run(
    config: AgentConfig,
    registry: Arc<ToolRegistry>,
    llm: &dyn Llm,
    sink: &mut dyn Sink,
) -> Result<RunSummary, AgentError> {
    let started_at = Timestamp::now();
    let system = config.context.system().clone();
    let goal = config.goal.clone();
    let model = config.model.clone();
    let tools = registry.specs().into_iter().map(|spec| spec.name).collect();
    sink.event(&Event::Started {
        at: started_at,
        goal,
        model,
        tools,
    });

    let (agent, mut request) = Agent::new(config).start();
    let mut agent = agent;
    loop {
        sink.event(&Event::ModelRequested {
            at: Timestamp::now(),
            turn: agent.turns_used(),
        });
        let response = llm.complete(&request).await?;
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
                let outputs = execute_batch(&registry, &batch, sink).await;
                let results = batch.complete(outputs)?;
                match waiting.tools_resolved(results)? {
                    ToolsTurn::NeedModel {
                        agent: next,
                        request: next_request,
                    } => {
                        agent = next;
                        request = next_request;
                    }
                    ToolsTurn::Done(finished) => return finish(finished, system, started_at, sink),
                }
            }
            ModelTurn::Done(finished) => return finish(finished, system, started_at, sink),
        }
    }
}

async fn execute_batch(
    registry: &ToolRegistry,
    batch: &ToolBatch,
    sink: &mut dyn Sink,
) -> Vec<ToolOutput> {
    let mut outputs = Vec::with_capacity(batch.calls().len());
    for call in batch.calls() {
        sink.event(&Event::ToolStarted {
            at: Timestamp::now(),
            call: call.clone(),
        });
        let output = registry.execute(call).await;
        sink.event(&Event::ToolFinished {
            at: Timestamp::now(),
            call_id: call.id.clone(),
            is_error: output.is_error,
        });
        outputs.push(output);
    }
    outputs
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
    use agnes_ports::tool::Tool;
    use agnes_util::NonEmpty;
    use async_trait::async_trait;
    use std::num::NonZeroU32;
    use std::str::FromStr;
    use std::sync::Mutex;

    /// Model that replays a fixed script of responses.
    struct ScriptedLlm {
        responses: Mutex<std::vec::IntoIter<LlmResponse>>,
    }

    impl ScriptedLlm {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter()),
            }
        }
    }

    #[async_trait]
    impl Llm for ScriptedLlm {
        async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse, LlmError> {
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
            model: ModelName::from_str("test-model").unwrap(),
            tools: Vec::new(),
            budget: TurnBudget::new(NonZeroU32::new(budget).unwrap()),
        }
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
        let summary = run(config(8), Arc::new(registry), &llm, &mut sink)
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
        let summary = run(config(8), Arc::new(registry), &llm, &mut sink)
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
            Arc::new(ToolRegistry::default()),
            &Broken,
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
}

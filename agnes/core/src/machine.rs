//! The agent as a type-state machine.
//!
//! Each state is a distinct type carrying its own data; transitions consume
//! the current state and return the next one together with the effect to
//! interpret (an [`LlmRequest`] or a [`ToolBatch`]). Illegal protocols —
//! running tools before the model asked for them, looping past the turn
//! budget — do not compile: [`AwaitingModel`] has no tool-running method and
//! [`Finished`] has no transitions at all.

use crate::config::{ModelName, TurnBudget};
use crate::context::Context;
use crate::llm::{LlmRequest, LlmResponse, StopReason};
use crate::message::{AssistantMessage, Message, TokenUsage, ToolCall, ToolOutput, Transcript};
use crate::tool::ToolSpec;

#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("model signalled tool use without any tool calls")]
    EmptyToolUse,
    #[error("tool results do not match the pending calls: expected {expected}, got {got}")]
    ResultMismatch { expected: usize, got: usize },
}

/// Everything needed to bring an agent up. `context` carries the system
/// prompt and prior turns; `tools` is the spec snapshot offered to the
/// model — executing calls is the host's job.
#[derive(Debug)]
pub struct AgentConfig {
    pub context: Context,
    pub goal: String,
    pub model: ModelName,
    pub tools: Vec<ToolSpec>,
    pub budget: TurnBudget,
}

/// The agent, generic over its protocol state.
#[derive(Debug)]
pub struct Agent<S> {
    state: S,
}

/// State: not started.
#[derive(Debug)]
pub struct Ready {
    context: Context,
    goal: String,
    model: ModelName,
    tools: Vec<ToolSpec>,
    budget: TurnBudget,
}

/// State: a model request is out, waiting for the response.
#[derive(Debug)]
pub struct AwaitingModel {
    transcript: Transcript,
    model: ModelName,
    specs: Vec<ToolSpec>,
    budget: TurnBudget,
    turns_used: u32,
    usage: TokenUsage,
}

/// State: the model asked for tools, waiting for their results.
#[derive(Debug)]
pub struct AwaitingTools {
    transcript: Transcript,
    model: ModelName,
    specs: Vec<ToolSpec>,
    budget: TurnBudget,
    turns_used: u32,
    usage: TokenUsage,
}

/// State: finished; no further transitions exist.
#[derive(Debug)]
pub struct Finished {
    transcript: Transcript,
    usage: TokenUsage,
    turns_used: u32,
    outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Completed { reply: String },
    BudgetExhausted,
    LengthExceeded,
}

/// The tool calls awaiting execution, handed out by [`ModelTurn::NeedTools`].
#[derive(Debug)]
pub struct ToolBatch {
    calls: Vec<ToolCall>,
}

impl ToolBatch {
    pub fn calls(&self) -> &[ToolCall] {
        &self.calls
    }

    /// Pairs results with the pending calls; the counts must match.
    pub fn complete(self, outputs: Vec<ToolOutput>) -> Result<ToolResults, MachineError> {
        if outputs.len() != self.calls.len() {
            return Err(MachineError::ResultMismatch {
                expected: self.calls.len(),
                got: outputs.len(),
            });
        }
        Ok(ToolResults {
            calls: self.calls,
            outputs,
        })
    }
}

/// One result per pending call, in call order. Constructible only via
/// [`ToolBatch::complete`].
#[derive(Debug)]
pub struct ToolResults {
    calls: Vec<ToolCall>,
    outputs: Vec<ToolOutput>,
}

impl ToolResults {
    pub fn iter(&self) -> impl Iterator<Item = (&ToolCall, &ToolOutput)> {
        self.calls.iter().zip(&self.outputs)
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

/// What to do after a model response.
#[derive(Debug)]
pub enum ModelTurn {
    /// The model asked for tools: run the batch, then feed the results back
    /// with [`Agent<AwaitingTools>::tools_resolved`].
    NeedTools {
        agent: Agent<AwaitingTools>,
        batch: ToolBatch,
    },
    /// The conversation is over.
    Done(Agent<Finished>),
}

/// What to do after tool results.
#[derive(Debug)]
pub enum ToolsTurn {
    /// Send the next request to the model.
    NeedModel {
        agent: Agent<AwaitingModel>,
        request: LlmRequest,
    },
    /// The budget is spent before the next model call.
    Done(Agent<Finished>),
}

impl Agent<Ready> {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            state: Ready {
                context: config.context,
                goal: config.goal,
                model: config.model,
                tools: config.tools,
                budget: config.budget,
            },
        }
    }

    /// Opens the conversation and produces the first model request.
    pub fn start(self) -> (Agent<AwaitingModel>, LlmRequest) {
        let Ready {
            context,
            goal,
            model,
            tools,
            budget,
        } = self.state;
        let transcript = context.into_transcript(goal);
        let request = build_request(&transcript, &model, &tools);
        (
            Agent {
                state: AwaitingModel {
                    transcript,
                    model,
                    specs: tools,
                    budget,
                    turns_used: 1,
                    usage: TokenUsage::default(),
                },
            },
            request,
        )
    }
}

impl Agent<AwaitingModel> {
    /// Feeds a model response back into the machine.
    pub fn model_responded(self, response: LlmResponse) -> Result<ModelTurn, MachineError> {
        let AwaitingModel {
            mut transcript,
            model,
            specs,
            budget,
            turns_used,
            usage,
        } = self.state;
        let usage = usage.saturating_add(response.usage);
        let AssistantMessage {
            content,
            tool_calls,
        } = response.assistant;
        match response.stop_reason {
            StopReason::ToolUse => {
                if tool_calls.is_empty() {
                    return Err(MachineError::EmptyToolUse);
                }
                transcript.push(Message::Assistant(AssistantMessage {
                    content,
                    tool_calls: tool_calls.clone(),
                }));
                Ok(ModelTurn::NeedTools {
                    agent: Agent {
                        state: AwaitingTools {
                            transcript,
                            model,
                            specs,
                            budget,
                            turns_used,
                            usage,
                        },
                    },
                    batch: ToolBatch { calls: tool_calls },
                })
            }
            StopReason::Completed => {
                let reply = content.clone();
                transcript.push(Message::Assistant(AssistantMessage {
                    content,
                    tool_calls,
                }));
                Ok(ModelTurn::Done(Agent {
                    state: Finished {
                        transcript,
                        usage,
                        turns_used,
                        outcome: Outcome::Completed { reply },
                    },
                }))
            }
            StopReason::LengthExceeded => {
                transcript.push(Message::Assistant(AssistantMessage {
                    content,
                    tool_calls,
                }));
                Ok(ModelTurn::Done(Agent {
                    state: Finished {
                        transcript,
                        usage,
                        turns_used,
                        outcome: Outcome::LengthExceeded,
                    },
                }))
            }
        }
    }

    pub fn turns_used(&self) -> u32 {
        self.state.turns_used
    }

    pub fn usage(&self) -> TokenUsage {
        self.state.usage
    }
}

impl Agent<AwaitingTools> {
    /// Feeds tool results back into the machine and produces the next model
    /// request — or finishes when the turn budget is spent.
    pub fn tools_resolved(self, results: ToolResults) -> Result<ToolsTurn, MachineError> {
        let AwaitingTools {
            mut transcript,
            model,
            specs,
            budget,
            turns_used,
            usage,
        } = self.state;
        for (call, output) in results.iter() {
            transcript.push(Message::ToolResult {
                call_id: call.id.clone(),
                output: output.clone(),
            });
        }
        let next_turn = turns_used + 1;
        if next_turn > budget.max_turns() {
            return Ok(ToolsTurn::Done(Agent {
                state: Finished {
                    transcript,
                    usage,
                    turns_used,
                    outcome: Outcome::BudgetExhausted,
                },
            }));
        }
        let request = build_request(&transcript, &model, &specs);
        Ok(ToolsTurn::NeedModel {
            agent: Agent {
                state: AwaitingModel {
                    transcript,
                    model,
                    specs,
                    budget,
                    turns_used: next_turn,
                    usage,
                },
            },
            request,
        })
    }
}

impl Agent<Finished> {
    pub fn outcome(&self) -> &Outcome {
        &self.state.outcome
    }

    pub fn usage(&self) -> TokenUsage {
        self.state.usage
    }

    pub fn turns_used(&self) -> u32 {
        self.state.turns_used
    }

    pub fn into_transcript(self) -> Transcript {
        self.state.transcript
    }
}

fn build_request(transcript: &Transcript, model: &ModelName, specs: &[ToolSpec]) -> LlmRequest {
    LlmRequest {
        model: model.clone(),
        messages: transcript.messages().to_vec(),
        tools: specs.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SystemPrompt, TurnBudget};
    use crate::message::{ToolArguments, ToolCallId};
    use crate::tool::ToolName;
    use std::num::NonZeroU32;
    use std::str::FromStr;

    fn config(budget: u32) -> AgentConfig {
        AgentConfig {
            context: Context::new(SystemPrompt::new("be brief")),
            goal: "flip the flag".to_owned(),
            model: ModelName::from_str("test-model").unwrap(),
            tools: Vec::new(),
            budget: TurnBudget::new(NonZeroU32::new(budget).unwrap()),
        }
    }

    fn response(
        stop_reason: StopReason,
        content: &str,
        calls: Vec<ToolCall>,
        usage: TokenUsage,
    ) -> LlmResponse {
        LlmResponse {
            assistant: AssistantMessage {
                content: content.to_owned(),
                tool_calls: calls,
            },
            usage,
            stop_reason,
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(id),
            name: ToolName::from_str(name).unwrap(),
            arguments: ToolArguments::new("{}"),
        }
    }

    fn output(ok: bool) -> ToolOutput {
        if ok {
            ToolOutput::success("done")
        } else {
            ToolOutput::error("boom")
        }
    }

    #[test]
    fn start_builds_transcript_and_request() {
        let (agent, request) = Agent::new(config(4)).start();
        assert_eq!(request.messages.len(), 2);
        assert!(matches!(request.messages[0], Message::System { .. }));
        assert!(matches!(request.messages[1], Message::User { .. }));
        assert_eq!(agent.turns_used(), 1);
        assert_eq!(agent.usage(), TokenUsage::default());
    }

    #[test]
    fn seeded_history_precedes_the_new_goal() {
        let context = Context::from_parts(
            SystemPrompt::new("be brief"),
            vec![
                Message::User {
                    content: "earlier task".to_owned(),
                },
                Message::Assistant(AssistantMessage {
                    content: "earlier reply".to_owned(),
                    tool_calls: vec![],
                }),
            ],
        );
        let config = AgentConfig {
            context,
            goal: "next task".to_owned(),
            ..config(4)
        };
        let (_agent, request) = Agent::new(config).start();

        assert_eq!(request.messages.len(), 4);
        assert!(matches!(request.messages[0], Message::System { .. }));
        assert!(
            matches!(request.messages[1], Message::User { ref content } if content == "earlier task")
        );
        assert!(matches!(request.messages[2], Message::Assistant(_)));
        assert!(
            matches!(request.messages[3], Message::User { ref content } if content == "next task")
        );
    }

    #[test]
    fn full_tool_flow_terminates() {
        let (agent, _request) = Agent::new(config(4)).start();
        let usage1 = TokenUsage {
            input: 10,
            output: 5,
        };
        let ModelTurn::NeedTools { agent, batch } = agent
            .model_responded(response(
                StopReason::ToolUse,
                "",
                vec![call("c1", "flag")],
                usage1,
            ))
            .unwrap()
        else {
            panic!("expected NeedTools");
        };
        assert_eq!(batch.calls().len(), 1);

        let results = batch.complete(vec![output(true)]).unwrap();
        let ToolsTurn::NeedModel { agent, request } = agent.tools_resolved(results).unwrap() else {
            panic!("expected NeedModel");
        };
        // system, user, assistant, tool result
        assert_eq!(request.messages.len(), 4);

        let usage2 = TokenUsage {
            input: 20,
            output: 8,
        };
        let ModelTurn::Done(finished) = agent
            .model_responded(response(StopReason::Completed, "done it", vec![], usage2))
            .unwrap()
        else {
            panic!("expected Done");
        };
        assert_eq!(
            finished.outcome(),
            &Outcome::Completed {
                reply: "done it".to_owned()
            }
        );
        assert_eq!(finished.turns_used(), 2);
        assert_eq!(
            finished.usage(),
            TokenUsage {
                input: 30,
                output: 13
            }
        );
        // system, user, assistant, tool result, assistant (final)
        assert_eq!(finished.into_transcript().into_messages().len(), 5);
    }

    #[test]
    fn budget_exhausts_before_next_model_call() {
        let (agent, _request) = Agent::new(config(1)).start();
        let ModelTurn::NeedTools { agent, batch } = agent
            .model_responded(response(
                StopReason::ToolUse,
                "",
                vec![call("c1", "flag")],
                TokenUsage::default(),
            ))
            .unwrap()
        else {
            panic!("expected NeedTools");
        };
        let results = batch.complete(vec![output(true)]).unwrap();
        let ToolsTurn::Done(finished) = agent.tools_resolved(results).unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(finished.outcome(), &Outcome::BudgetExhausted);
    }

    #[test]
    fn tool_use_without_calls_is_malformed() {
        let (agent, _request) = Agent::new(config(4)).start();
        let err = agent
            .model_responded(response(
                StopReason::ToolUse,
                "",
                vec![],
                TokenUsage::default(),
            ))
            .unwrap_err();
        assert!(matches!(err, MachineError::EmptyToolUse));
    }

    #[test]
    fn result_count_must_match_batch() {
        let (agent, _request) = Agent::new(config(4)).start();
        let ModelTurn::NeedTools { agent: _, batch } = agent
            .model_responded(response(
                StopReason::ToolUse,
                "",
                vec![call("c1", "flag"), call("c2", "flag")],
                TokenUsage::default(),
            ))
            .unwrap()
        else {
            panic!("expected NeedTools");
        };
        let err = batch.complete(vec![output(true)]).unwrap_err();
        assert!(matches!(
            err,
            MachineError::ResultMismatch {
                expected: 2,
                got: 1
            }
        ));
    }

    #[test]
    fn length_exceeded_finishes() {
        let (agent, _request) = Agent::new(config(4)).start();
        let ModelTurn::Done(finished) = agent
            .model_responded(response(
                StopReason::LengthExceeded,
                "partial",
                vec![],
                TokenUsage::default(),
            ))
            .unwrap()
        else {
            panic!("expected Done");
        };
        assert_eq!(finished.outcome(), &Outcome::LengthExceeded);
    }
}

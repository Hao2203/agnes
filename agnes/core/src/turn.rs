//! The agent run, progressing one turn at a time as a type-state machine.
//!
//! Each state is a distinct type carrying its own data; transitions consume
//! the current state and return the next one together with the effect to
//! interpret (a [`Prompt`] or a [`ToolBatch`]). Illegal protocols —
//! running tools before the model asked for them, looping past the turn
//! budget — do not compile: [`AwaitingModel`] has no tool-running method and
//! [`Finished`] has no transitions at all.
//!
//! The machine knows the conversation and the tools on offer, never the
//! model: it hands out [`Prompt`]s, and the host turns each one into a
//! concrete request.
//!
//! Transitions are fed by the host. Besides model responses and tool
//! results, the user acts out of band: messages typed while the run works
//! enter at the one protocol-safe boundary — after a tool batch — through
//! [`Agent<AwaitingTools>::tools_resolved`], and an interruption winds the
//! run down from either awaiting state, yielding [`Outcome::Interrupted`]
//! with the transcript kept intact.

use crate::config::TurnBudget;
use crate::content::Content;
use crate::context::Context;
use crate::llm::{LlmResponse, Prompt, StopReason};
use crate::message::{AssistantMessage, Message, TokenUsage, ToolCall, ToolOutput, Transcript};
use crate::tool::ToolSpec;
use agnes_util::NonEmpty;

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("tool results do not match the pending calls: expected {expected}, got {got}")]
    ResultMismatch { expected: usize, got: usize },
}

/// Everything needed to bring an agent up. `context` carries the system
/// prompt and prior turns; `tools` is the spec snapshot offered to the
/// model — executing calls is the host's job. The model itself is the
/// host's concern: it attaches one to each [`Prompt`].
#[derive(Debug)]
pub struct AgentConfig {
    pub context: Context,
    pub goal: Content,
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
    goal: Content,
    tools: Vec<ToolSpec>,
    budget: TurnBudget,
}

/// State: a model request is out, waiting for the response.
#[derive(Debug)]
pub struct AwaitingModel {
    transcript: Transcript,
    specs: Vec<ToolSpec>,
    budget: TurnBudget,
    turns_used: u32,
    usage: TokenUsage,
}

/// State: the model asked for tools, waiting for their results.
#[derive(Debug)]
pub struct AwaitingTools {
    transcript: Transcript,
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
    Completed {
        reply: Content,
    },
    /// The user interrupted the run; the transcript holds every turn up to
    /// the interruption.
    Interrupted,
    BudgetExhausted,
    LengthExceeded,
}

/// The tool calls awaiting execution, handed out by [`ModelTurn::NeedTools`].
/// Always holds at least one call.
#[derive(Debug)]
pub struct ToolBatch {
    calls: NonEmpty<ToolCall>,
}

impl ToolBatch {
    pub fn calls(&self) -> &[ToolCall] {
        self.calls.as_ref()
    }

    /// Pairs each pending call with its result; the counts must match.
    pub fn complete(self, outputs: Vec<ToolOutput>) -> Result<ToolResults, TurnError> {
        if outputs.len() != self.calls.len() {
            return Err(TurnError::ResultMismatch {
                expected: self.calls.len(),
                got: outputs.len(),
            });
        }
        Ok(ToolResults {
            pairs: NonEmpty::from_iter(self.calls.iter().cloned().zip(outputs))
                .expect("a non-empty batch with matching counts yields at least one pair"),
        })
    }
}

/// One result per pending call, in call order; the pairing is structural.
/// Constructible only via [`ToolBatch::complete`].
#[derive(Debug)]
pub struct ToolResults {
    pairs: NonEmpty<(ToolCall, ToolOutput)>,
}

impl ToolResults {
    pub fn iter(&self) -> impl Iterator<Item = (&ToolCall, &ToolOutput)> {
        self.pairs.iter().map(|(call, output)| (call, output))
    }

    /// Number of results; always at least one — a [`ToolResults`] is only
    /// produced for a [`ToolBatch`], which holds at least one call.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.pairs.len()
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
    /// Turn the next prompt into a request and send it to the model.
    NeedModel {
        agent: Agent<AwaitingModel>,
        prompt: Prompt,
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
                tools: config.tools,
                budget: config.budget,
            },
        }
    }

    /// Opens the conversation and produces the first prompt.
    pub fn start(self) -> (Agent<AwaitingModel>, Prompt) {
        let Ready {
            context,
            goal,
            tools,
            budget,
        } = self.state;
        let transcript = context.into_transcript(goal);
        let prompt = build_prompt(&transcript, &tools);
        (
            Agent {
                state: AwaitingModel {
                    transcript,
                    specs: tools,
                    budget,
                    turns_used: 1,
                    usage: TokenUsage::default(),
                },
            },
            prompt,
        )
    }
}

impl Agent<AwaitingModel> {
    /// Feeds a model response back into the machine.
    pub fn model_responded(self, response: LlmResponse) -> Result<ModelTurn, TurnError> {
        let AwaitingModel {
            mut transcript,
            specs,
            budget,
            turns_used,
            usage,
        } = self.state;
        let usage = usage.saturating_add(response.usage);
        match response.stop_reason {
            StopReason::ToolUse { content, calls } => {
                let batch = ToolBatch {
                    calls: calls.clone(),
                };
                transcript.push(Message::Assistant(AssistantMessage::ToolCalls {
                    content,
                    calls,
                }));
                Ok(ModelTurn::NeedTools {
                    agent: Agent {
                        state: AwaitingTools {
                            transcript,
                            specs,
                            budget,
                            turns_used,
                            usage,
                        },
                    },
                    batch,
                })
            }
            StopReason::Completed { content } => {
                transcript.push(Message::Assistant(AssistantMessage::Text(content.clone())));
                Ok(ModelTurn::Done(Agent {
                    state: Finished {
                        transcript,
                        usage,
                        turns_used,
                        outcome: Outcome::Completed { reply: content },
                    },
                }))
            }
            StopReason::LengthExceeded { content } => {
                if let Some(content) = content {
                    transcript.push(Message::Assistant(AssistantMessage::Text(content)));
                }
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

    /// Winds the run down without a model response: the user interrupted
    /// while the request was in flight. The transcript keeps what it had —
    /// its last entry is a user message or a tool result, so it stays valid
    /// for later requests.
    pub fn interrupted(self) -> Agent<Finished> {
        let AwaitingModel {
            transcript,
            usage,
            turns_used,
            ..
        } = self.state;
        Agent {
            state: Finished {
                transcript,
                usage,
                turns_used,
                outcome: Outcome::Interrupted,
            },
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
    /// Feeds tool results back into the machine and produces the next
    /// prompt — or finishes when the turn budget is spent. Messages the
    /// user typed while the run was working ride along: they are appended
    /// after the results, in order, so the next prompt carries them.
    pub fn tools_resolved(
        self,
        results: ToolResults,
        user_messages: Vec<Content>,
    ) -> Result<ToolsTurn, TurnError> {
        let AwaitingTools {
            mut transcript,
            specs,
            budget,
            turns_used,
            usage,
        } = self.state;
        absorb(&mut transcript, results);
        for content in user_messages {
            transcript.push(Message::User { content });
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
        let prompt = build_prompt(&transcript, &specs);
        Ok(ToolsTurn::NeedModel {
            agent: Agent {
                state: AwaitingModel {
                    transcript,
                    specs,
                    budget,
                    turns_used: next_turn,
                    usage,
                },
            },
            prompt,
        })
    }

    /// Winds the run down after an interruption: `results` must cover every
    /// pending call — real outputs for the calls that ran, synthetic
    /// "interrupted" outputs for the rest — so the transcript never keeps a
    /// tool call without its result.
    pub fn interrupted(self, results: ToolResults) -> Agent<Finished> {
        let AwaitingTools {
            mut transcript,
            usage,
            turns_used,
            ..
        } = self.state;
        absorb(&mut transcript, results);
        Agent {
            state: Finished {
                transcript,
                usage,
                turns_used,
                outcome: Outcome::Interrupted,
            },
        }
    }
}

/// Appends one tool result per pending call, in call order.
fn absorb(transcript: &mut Transcript, results: ToolResults) {
    for (call, output) in results.iter() {
        transcript.push(Message::ToolResult {
            call_id: call.id.clone(),
            output: output.clone(),
        });
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

/// Snapshots the transcript and the tool specs into the semantic model
/// input; attaching a model to it is the host's job.
fn build_prompt(transcript: &Transcript, specs: &[ToolSpec]) -> Prompt {
    Prompt {
        input: transcript.messages().to_vec(),
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

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    fn config(budget: u32) -> AgentConfig {
        AgentConfig {
            context: Context::new(SystemPrompt::new("be brief")),
            goal: content("flip the flag"),
            tools: Vec::new(),
            budget: TurnBudget::new(NonZeroU32::new(budget).unwrap()),
        }
    }

    fn response(stop_reason: StopReason, usage: TokenUsage) -> LlmResponse {
        LlmResponse { stop_reason, usage }
    }

    fn completed(text: &str) -> StopReason {
        StopReason::Completed {
            content: content(text),
        }
    }

    fn tool_use(calls: Vec<ToolCall>) -> StopReason {
        StopReason::ToolUse {
            content: None,
            calls: NonEmpty::from_vec(calls).unwrap(),
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
    fn start_builds_transcript_and_prompt() {
        let (agent, prompt) = Agent::new(config(4)).start();
        assert_eq!(prompt.input.len(), 2);
        assert!(matches!(prompt.input[0], Message::System { .. }));
        assert!(matches!(prompt.input[1], Message::User { .. }));
        assert!(prompt.tools.is_empty());
        assert_eq!(agent.turns_used(), 1);
        assert_eq!(agent.usage(), TokenUsage::default());
    }

    #[test]
    fn seeded_history_precedes_the_new_goal() {
        let context = Context::from_parts(
            SystemPrompt::new("be brief"),
            vec![
                Message::User {
                    content: content("earlier task"),
                },
                Message::Assistant(AssistantMessage::Text(content("earlier reply"))),
            ],
        );
        let config = AgentConfig {
            context,
            goal: content("next task"),
            ..config(4)
        };
        let (_agent, prompt) = Agent::new(config).start();

        assert_eq!(prompt.input.len(), 4);
        assert!(matches!(prompt.input[0], Message::System { .. }));
        assert!(
            matches!(prompt.input[1], Message::User { ref content } if content.as_str() == "earlier task")
        );
        assert!(matches!(prompt.input[2], Message::Assistant(_)));
        assert!(
            matches!(prompt.input[3], Message::User { ref content } if content.as_str() == "next task")
        );
    }

    #[test]
    fn full_tool_flow_terminates() {
        let (agent, _prompt) = Agent::new(config(4)).start();
        let usage1 = TokenUsage {
            input: 10,
            output: 5,
        };
        let ModelTurn::NeedTools { agent, batch } = agent
            .model_responded(response(tool_use(vec![call("c1", "flag")]), usage1))
            .unwrap()
        else {
            panic!("expected NeedTools");
        };
        assert_eq!(batch.calls().len(), 1);

        let results = batch.complete(vec![output(true)]).unwrap();
        let ToolsTurn::NeedModel { agent, prompt } =
            agent.tools_resolved(results, Vec::new()).unwrap()
        else {
            panic!("expected NeedModel");
        };
        // system, user, assistant, tool result
        assert_eq!(prompt.input.len(), 4);

        let usage2 = TokenUsage {
            input: 20,
            output: 8,
        };
        let ModelTurn::Done(finished) = agent
            .model_responded(response(completed("done it"), usage2))
            .unwrap()
        else {
            panic!("expected Done");
        };
        assert_eq!(
            finished.outcome(),
            &Outcome::Completed {
                reply: content("done it")
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
        let (agent, _prompt) = Agent::new(config(1)).start();
        let ModelTurn::NeedTools { agent, batch } = agent
            .model_responded(response(
                tool_use(vec![call("c1", "flag")]),
                TokenUsage::default(),
            ))
            .unwrap()
        else {
            panic!("expected NeedTools");
        };
        let results = batch.complete(vec![output(true)]).unwrap();
        let ToolsTurn::Done(finished) = agent.tools_resolved(results, Vec::new()).unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(finished.outcome(), &Outcome::BudgetExhausted);
    }

    #[test]
    fn result_count_must_match_batch() {
        let (agent, _prompt) = Agent::new(config(4)).start();
        let ModelTurn::NeedTools { agent: _, batch } = agent
            .model_responded(response(
                tool_use(vec![call("c1", "flag"), call("c2", "flag")]),
                TokenUsage::default(),
            ))
            .unwrap()
        else {
            panic!("expected NeedTools");
        };
        let err = batch.complete(vec![output(true)]).unwrap_err();
        assert!(matches!(
            err,
            TurnError::ResultMismatch {
                expected: 2,
                got: 1
            }
        ));
    }

    #[test]
    fn length_exceeded_finishes() {
        let (agent, _prompt) = Agent::new(config(4)).start();
        let ModelTurn::Done(finished) = agent
            .model_responded(response(
                StopReason::LengthExceeded {
                    content: Some(content("partial")),
                },
                TokenUsage::default(),
            ))
            .unwrap()
        else {
            panic!("expected Done");
        };
        assert_eq!(finished.outcome(), &Outcome::LengthExceeded);
    }

    #[test]
    fn queued_user_messages_are_injected_after_tool_results() {
        let (agent, _prompt) = Agent::new(config(4)).start();
        let ModelTurn::NeedTools { agent, batch } = agent
            .model_responded(response(
                tool_use(vec![call("c1", "flag")]),
                TokenUsage::default(),
            ))
            .unwrap()
        else {
            panic!("expected NeedTools");
        };
        let results = batch.complete(vec![output(true)]).unwrap();
        let ToolsTurn::NeedModel { prompt, .. } = agent
            .tools_resolved(
                results,
                vec![content("also check the tests"), content("and the docs")],
            )
            .unwrap()
        else {
            panic!("expected NeedModel");
        };
        // system, goal, assistant (tool calls), tool result, user, user
        assert_eq!(prompt.input.len(), 6);
        assert!(matches!(&prompt.input[3], Message::ToolResult { .. }));
        assert!(matches!(
            &prompt.input[4],
            Message::User { content } if content.as_str() == "also check the tests"
        ));
        assert!(matches!(
            &prompt.input[5],
            Message::User { content } if content.as_str() == "and the docs"
        ));
    }

    #[test]
    fn injected_messages_ride_along_when_the_budget_ends_the_run() {
        let (agent, _prompt) = Agent::new(config(1)).start();
        let ModelTurn::NeedTools { agent, batch } = agent
            .model_responded(response(
                tool_use(vec![call("c1", "flag")]),
                TokenUsage::default(),
            ))
            .unwrap()
        else {
            panic!("expected NeedTools");
        };
        let results = batch.complete(vec![output(true)]).unwrap();
        let ToolsTurn::Done(finished) = agent
            .tools_resolved(results, vec![content("keep this")])
            .unwrap()
        else {
            panic!("expected Done");
        };
        assert_eq!(finished.outcome(), &Outcome::BudgetExhausted);
        let messages = finished.into_transcript().into_messages();
        // system, goal, assistant, tool result, injected user
        assert_eq!(messages.len(), 5);
        assert!(matches!(
            &messages[4],
            Message::User { content } if content.as_str() == "keep this"
        ));
    }

    #[test]
    fn interrupt_while_awaiting_the_model_keeps_the_transcript() {
        let (agent, _prompt) = Agent::new(config(4)).start();
        let finished = agent.interrupted();
        assert_eq!(finished.outcome(), &Outcome::Interrupted);
        assert_eq!(finished.turns_used(), 1);
        assert_eq!(finished.usage(), TokenUsage::default());
        // just system and goal — the lost response was never recorded
        assert_eq!(finished.into_transcript().into_messages().len(), 2);
    }

    #[test]
    fn interrupt_while_awaiting_tools_records_every_result() {
        let (agent, _prompt) = Agent::new(config(4)).start();
        let ModelTurn::NeedTools { agent, batch } = agent
            .model_responded(response(
                tool_use(vec![call("c1", "flag"), call("c2", "flag")]),
                TokenUsage::default(),
            ))
            .unwrap()
        else {
            panic!("expected NeedTools");
        };
        let results = batch
            .complete(vec![
                output(true),
                ToolOutput::error("not run: the user interrupted the agent"),
            ])
            .unwrap();
        let finished = agent.interrupted(results);
        assert_eq!(finished.outcome(), &Outcome::Interrupted);
        assert_eq!(finished.turns_used(), 1);
        let messages = finished.into_transcript().into_messages();
        // system, goal, assistant (tool calls), tool result, tool result
        assert_eq!(messages.len(), 5);
        assert!(matches!(
            &messages[3],
            Message::ToolResult { output, .. } if !output.is_error
        ));
        assert!(matches!(
            &messages[4],
            Message::ToolResult { output, .. } if output.is_error
        ));
    }
}

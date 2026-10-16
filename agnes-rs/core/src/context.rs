//! Session context: the single state carrier of a run — the system prompt, the
//! environment the run acts in, and one ordered log of every runtime event.
//!
//! Messages are events: the goal, injected user messages, model messages and
//! tool results all enter the log as [`Event::Message`], so [`Context::events`]
//! is the complete, ordered history of a session while [`Context::messages`] is
//! the projection a model request is usually built from. What that request
//! looks like is the host's call — see [`crate::llm::RequestBuilder`].
//!
//! The state machine records into the context as it advances; hosts read it
//! and hand it back to continue a session.

use crate::config::SystemPrompt;
use crate::event::Event;
use crate::message::Message;
use crate::tool::ToolSpec;

/// What the run can act with: the tools on offer. Grows with the needs of
/// hosts.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    tools: Vec<ToolSpec>,
}

impl Environment {
    pub fn new(tools: Vec<ToolSpec>) -> Self {
        Self { tools }
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// Replaces the tools on offer.
    pub fn set_tools(&mut self, tools: Vec<ToolSpec>) {
        self.tools = tools;
    }
}

/// The system prompt, the environment, and every runtime event in order.
#[derive(Debug, Clone)]
pub struct Context {
    system: SystemPrompt,
    environment: Environment,
    events: Vec<Event>,
}

impl Context {
    pub fn new(system: SystemPrompt, environment: Environment) -> Self {
        Self {
            system,
            environment,
            events: Vec::new(),
        }
    }

    pub fn system(&self) -> &SystemPrompt {
        &self.system
    }

    pub fn environment(&self) -> &Environment {
        &self.environment
    }

    /// The environment is session state: a host may swap the tools on offer
    /// between runs.
    pub fn environment_mut(&mut self) -> &mut Environment {
        &mut self.environment
    }

    /// Every runtime event, in order — messages included.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The conversation messages, in order: the log projected onto
    /// [`Event::Message`].
    pub fn messages(&self) -> impl Iterator<Item = &Message> + '_ {
        self.events.iter().filter_map(|event| match event {
            Event::Message(message) => Some(message),
            _ => None,
        })
    }

    /// Starts a fresh session: the log is dropped; the system prompt and the
    /// environment are kept.
    pub fn reset(&mut self) {
        self.events.clear();
    }

    /// Appends an event to the log. Only the state machine records.
    pub(crate) fn record(&mut self, event: Event) {
        self.events.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::Content;
    use crate::message::AssistantMessage;
    use crate::tool::{JsonSchema, ToolName};
    use crate::turn::Outcome;
    use std::str::FromStr;

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    fn user_event(text: &str) -> Event {
        Event::Message(Message::User {
            content: content(text),
        })
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_str(name).unwrap(),
            description: "".into(),
            parameters: JsonSchema::try_from(serde_json::json!({"type": "object"})).unwrap(),
        }
    }

    #[test]
    fn records_events_in_order() {
        let mut context = Context::new(SystemPrompt::new("sys"), Environment::default());
        context.record(Event::TurnStart);
        context.record(user_event("hello"));
        context.record(Event::TurnEnd {
            outcome: Outcome::Interrupted,
        });

        assert_eq!(context.events().len(), 3);
        assert!(matches!(context.events()[0], Event::TurnStart));
        assert!(matches!(context.events()[1], Event::Message(_)));
        assert!(matches!(context.events()[2], Event::TurnEnd { .. }));
    }

    #[test]
    fn messages_are_the_projection_of_the_log() {
        let mut context = Context::new(SystemPrompt::new("sys"), Environment::default());
        context.record(Event::TurnStart);
        context.record(user_event("first"));
        context.record(Event::Message(Message::Assistant(AssistantMessage::Text(
            content("reply"),
        ))));

        let messages: Vec<&Message> = context.messages().collect();
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0], Message::User { content } if content.as_str() == "first"));
        assert!(matches!(messages[1], Message::Assistant(_)));
    }

    #[test]
    fn reset_drops_the_log_but_keeps_system_and_environment() {
        let mut context = Context::new(
            SystemPrompt::new("sys"),
            Environment::new(vec![spec("note")]),
        );
        context.record(user_event("old"));
        assert_eq!(context.events().len(), 1);

        context.reset();
        assert!(context.events().is_empty());
        assert_eq!(context.system().as_str(), "sys");
        assert_eq!(context.environment().tools().len(), 1);
    }

    #[test]
    fn the_environment_tools_can_be_replaced() {
        let mut environment = Environment::default();
        assert!(environment.tools().is_empty());
        environment.set_tools(vec![spec("note"), spec("read_file")]);
        let names: Vec<&str> = environment
            .tools()
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert_eq!(names, vec!["note", "read_file"]);
    }
}

//! The session log: the single state of a run — an ordered sequence of
//! [`Event`]s.
//!
//! Messages, the session header (system prompt and tool environment, see
//! [`crate::event::Event::Header`]) and the step/turn lifecycle are all
//! events, so the log *is* the history: [`Log::messages`] is the projection a
//! model request is built from and [`Log::header`] is the configuration that
//! was in force most recently. There is no state outside the log.
//!
//! The state machine is the only writer (`record` is not public); hosts read
//! the log and hand it back to continue a session. What a request looks like
//! is the host's call — see [`crate::llm::RequestBuilder`].

use crate::event::Event;
use crate::header::Header;
use crate::message::Message;

/// The ordered log of one session.
#[derive(Debug, Clone, Default)]
pub struct Log {
    events: Vec<Event>,
}

impl Log {
    /// Rebuilds a session from an already-validated log.
    ///
    /// This is the replay boundary, not a recording path: the caller (a
    /// session store) has checked continuity and ordering, and the machine
    /// keeps appending through its own transitions. Core trusts typed
    /// in-process input — validation belongs at the file/wire boundary.
    pub fn restore(events: Vec<Event>) -> Self {
        Self { events }
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

    /// The header that was in force most recently — the system prompt and the
    /// tools the model was last offered.
    pub fn header(&self) -> Option<&Header> {
        self.events.iter().rev().find_map(|event| match event {
            Event::Header(header) => Some(header),
            _ => None,
        })
    }

    /// Starts a fresh session: the log is dropped. The next turn records its
    /// own header, so nothing needs to be carried over.
    pub fn reset(&mut self) {
        self.events.clear();
    }

    /// Appends an event to the log, returning its position. Only the state
    /// machine records; the position is what notifications carry, so the log
    /// stays the single owner of the event.
    pub(crate) fn record(&mut self, event: Event) -> EventIndex {
        let index = EventIndex(self.events.len());
        self.events.push(event);
        index
    }

    /// Reads one logged event by position.
    pub fn event(&self, index: EventIndex) -> Option<&Event> {
        self.events.get(index.0)
    }
}

/// A position in the log, counted from zero.
///
/// Machine actions carry this instead of a copy of the event: the log is the
/// only owner, and a notification cannot drift from what was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EventIndex(usize);

impl EventIndex {
    /// The position, counted from zero.
    pub fn get(self) -> usize {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemPrompt;
    use crate::content::Content;
    use crate::event::Event;
    use crate::header::Header;
    use crate::message::AssistantMessage;
    use crate::turn::Outcome;

    fn content(text: &str) -> Content {
        Content::try_from(text).unwrap()
    }

    fn user_event(text: &str) -> Event {
        Event::Message(Message::User {
            content: content(text),
        })
    }

    #[test]
    fn records_events_in_order() {
        let mut log = Log::default();
        let first = log.record(Event::TurnStart);
        let second = log.record(user_event("hello"));
        let third = log.record(Event::TurnEnd {
            outcome: Outcome::Interrupted,
        });

        assert_eq!((first.get(), second.get(), third.get()), (0, 1, 2));
        assert_eq!(log.events().len(), 3);
        assert!(matches!(log.events()[0], Event::TurnStart));
        assert!(matches!(log.events()[1], Event::Message(_)));
        assert!(matches!(log.events()[2], Event::TurnEnd { .. }));
    }

    #[test]
    fn messages_are_the_projection_of_the_log() {
        let mut log = Log::default();
        log.record(Event::TurnStart);
        log.record(user_event("first"));
        log.record(Event::Message(Message::Assistant(AssistantMessage::Text(
            content("reply"),
        ))));

        let messages: Vec<&Message> = log.messages().collect();
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0], Message::User { content } if content.as_str() == "first"));
        assert!(matches!(messages[1], Message::Assistant(_)));
    }

    #[test]
    fn reset_drops_the_log() {
        let mut log = Log::default();
        log.record(user_event("old"));
        assert_eq!(log.events().len(), 1);

        log.reset();
        assert!(log.events().is_empty());
        assert!(log.header().is_none());
    }

    #[test]
    fn restore_seeds_the_log_from_a_validated_history() {
        let events = vec![Event::TurnStart, user_event("hello")];
        let log = Log::restore(events.clone());

        assert_eq!(log.events(), events.as_slice());
        assert_eq!(log.messages().count(), 1);
    }

    #[test]
    fn the_header_is_the_most_recent_one() {
        let log = Log::restore(vec![
            Event::Header(Header::new(
                SystemPrompt::new("old"),
                crate::header::Environment::default(),
            )),
            user_event("hello"),
            Event::Header(Header::new(
                SystemPrompt::new("new"),
                crate::header::Environment::default(),
            )),
        ]);

        assert_eq!(log.header().unwrap().system.as_str(), "new");
    }
}

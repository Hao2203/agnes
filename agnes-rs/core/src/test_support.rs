//! Shared fixtures for core unit tests.

use crate::config::{StepBudget, SystemPrompt};
use crate::content::Content;
use crate::header::{Environment, Header};
use crate::message::Message;
use std::num::NonZeroU32;

pub(crate) fn content(text: &str) -> Content {
    Content::try_from(text).unwrap()
}

pub(crate) fn header() -> Header {
    Header::new(SystemPrompt::new("be brief"), Environment::default())
}

pub(crate) fn budget(steps: u32) -> StepBudget {
    StepBudget::new(NonZeroU32::new(steps).unwrap())
}

pub(crate) fn user(text: &str) -> Message {
    Message::User {
        content: content(text),
    }
}

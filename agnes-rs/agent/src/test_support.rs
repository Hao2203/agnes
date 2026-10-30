//! 测试共享的消息构造器。

use agnes_core::content::Content;
use agnes_core::message::{AssistantMessage, Message};

pub(crate) fn user(text: &str) -> Message {
    Message::User {
        content: Content::try_from(text).unwrap(),
    }
}

pub(crate) fn assistant(text: &str) -> Message {
    Message::Assistant(AssistantMessage::Text(Content::try_from(text).unwrap()))
}

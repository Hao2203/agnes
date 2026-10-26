//! 会话日志：一次运行的唯一状态 —— 按发生顺序排列的事件序列。
//!
//! 消息、会话配置（系统提示与工具环境，见 [`Event::Header`]）以及
//! step/turn 生命周期都是事件，所以日志就是历史：[`Log::messages`] 是
//! 构造模型请求的投影，[`Log::header`] 是最近一次生效的配置。日志之外
//! 没有状态。
//!
//! core 的状态机只产生事件动作；驱动是唯一的写者，并把日志交还给宿主
//! 以延续会话。请求长什么样是宿主的事 —— 由 [`crate::RequestBuilder`]
//! 决定。

use agnes_core::event::Event;
use agnes_core::header::Header;
use agnes_core::message::Message;

/// 一个会话的有序日志。
#[derive(Debug, Clone, Default)]
pub struct Log {
    events: Vec<Event>,
}

impl Log {
    /// 从已经校验过的历史重建会话。
    ///
    /// 这是重放边界，不是记录路径：调用方（会话存储）已经检查过连续性
    /// 与顺序；驱动随后继续追加机器产生的事件。core 信任进程内的强类型
    /// 输入 —— 校验属于文件/线缆边界。
    pub fn restore(events: Vec<Event>) -> Self {
        Self { events }
    }

    /// 每条运行时事件，按顺序 —— 消息也在其中。
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// 会话消息，按顺序：日志投影到 [`Event::Message`]。
    pub fn messages(&self) -> impl Iterator<Item = &Message> + '_ {
        self.events.iter().filter_map(|event| match event {
            Event::Message(message) => Some(message),
            _ => None,
        })
    }

    /// 最近一次生效的配置 —— 系统提示与模型上一轮拿到的工具表。
    pub fn header(&self) -> Option<&Header> {
        self.events.iter().rev().find_map(|event| match event {
            Event::Header(header) => Some(header),
            _ => None,
        })
    }

    /// 开始新会话：日志清空。下一个 turn 会记录自己的 header，所以无需
    /// 携带任何旧状态。
    pub fn reset(&mut self) {
        self.events.clear();
    }

    /// 追加一条事件。驱动记录机器以动作形式产生的事件；机器本身不持有
    /// 历史。
    pub(crate) fn record(&mut self, event: Event) -> &Event {
        self.events.push(event);
        self.events
            .last()
            .expect("record just pushed an event onto the log")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::config::SystemPrompt;
    use agnes_core::content::Content;
    use agnes_core::message::AssistantMessage;
    use agnes_core::turn::Outcome;

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
        log.record(Event::TurnStart);
        log.record(user_event("hello"));
        log.record(Event::TurnEnd {
            outcome: Outcome::Interrupted,
        });

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
                agnes_core::header::Environment::default(),
            )),
            user_event("hello"),
            Event::Header(Header::new(
                SystemPrompt::new("new"),
                agnes_core::header::Environment::default(),
            )),
        ]);

        assert_eq!(log.header().unwrap().system().as_str(), "new");
    }
}

//! 审批服务：宿主自有的问询基础设施。
//!
//! 门禁（运行时系统）只需表达「要审批」；问询按
//! [`ApprovalChannel`] 递给审批界面。答覆经 `RuntimeSender` 以
//! `agnes_agent::runtime::ApprovalDecided` 消息回注；落定词汇
//! ([`agnes_record::ApprovalOutcome`]) 是 fail-closed 的 —— 通道关闭、
//! 答覆被丢弃、超时、运行被打断，都按拒绝落定。

use agnes_core::message::{ToolArguments, ToolCallId};
use agnes_core::tool::ToolName;
use agnes_ecs::Entity;
use std::time::Duration;
use tokio::sync::mpsc;

/// 递给审批界面的一次问询：界面收到后决定批准与否；答覆以 ECS 消息
/// （`ApprovalDecided`）回注。落定词汇
/// ([`agnes_record::ApprovalOutcome`]) 是 fail-closed 的 —— 通道关闭、
/// 答覆被丢弃、超时、运行被打断，都按拒绝落定。
#[derive(Debug)]
pub struct ApprovalRequest {
    /// 问询属于哪个 agent；`call_id` 只在 agent 内唯一。
    pub agent: Entity,
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: ToolArguments,
}

/// 问询通道：门禁按它发问询；审批界面持有配对的 [`mpsc::Receiver`]。
/// `Clone` 后可全班共享。
///
/// 容量为 1：同一时刻只允许一个 in-flight 问询；队列满或接收端消失
/// 都按 fail-closed 处理，避免无界缓冲掩盖这一契约。
#[derive(Debug, Clone)]
pub struct ApprovalChannel {
    tx: mpsc::Sender<ApprovalRequest>,
    /// 问询等待答覆的上限；超时按拒绝处理。不设则一直等（或打断）。
    timeout: Option<Duration>,
}

impl ApprovalChannel {
    /// 建一对（发送端，界面接收端）；`timeout` 作用于每次问询。
    pub fn channel(timeout: Option<Duration>) -> (Self, mpsc::Receiver<ApprovalRequest>) {
        let (tx, rx) = mpsc::channel(1);
        (Self { tx, timeout }, rx)
    }

    /// 递送一次问询；接口端已消失或已有在飞问询则失败（门禁按
    /// fail-closed 落定）。
    ///
    /// # Errors
    ///
    /// 接收端已关闭，或已有一个问询在飞。
    pub fn send(
        &self,
        request: ApprovalRequest,
    ) -> Result<(), mpsc::error::TrySendError<ApprovalRequest>> {
        self.tx.try_send(request)
    }

    /// 一次问询的等待上限。
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::message::ToolArguments;
    use agnes_core::tool::ToolName;
    use std::str::FromStr;

    #[tokio::test]
    async fn a_question_travels_to_the_answerer() {
        let (channel, mut questions) = ApprovalChannel::channel(Some(Duration::from_secs(3)));
        let request = ApprovalRequest {
            agent: Entity::PLACEHOLDER,
            call_id: ToolCallId::new("c1"),
            tool_name: ToolName::from_str("run_shell").unwrap(),
            arguments: ToolArguments::new("{}"),
        };
        channel.send(request).expect("the question fits the queue");

        let question = questions.recv().await.expect("question received");
        assert_eq!(question.call_id.as_str(), "c1");
        assert_eq!(channel.timeout(), Some(Duration::from_secs(3)));
    }

    #[tokio::test]
    async fn a_second_in_flight_question_is_refused() {
        let (channel, mut questions) = ApprovalChannel::channel(None);
        let make = || ApprovalRequest {
            agent: Entity::PLACEHOLDER,
            call_id: ToolCallId::new("c1"),
            tool_name: ToolName::from_str("run_shell").unwrap(),
            arguments: ToolArguments::new("{}"),
        };
        channel.send(make()).expect("first question fits");
        assert!(channel.send(make()).is_err(), "capacity is one");
        questions.close();
    }
}

//! 审批服务：系统自有的问询基础设施。
//!
//! 插件只发 [`Command::Ask`](crate::plugin::Command::Ask) 指令；驱动按照
//! 本服务发问询（每问询自带一条 oneshot 答覆通道）、挂起等待
//! （可选超时）、按答覆执行——审批的 IO 与等待全部在宿主侧，
//! core 与插件都不需要知道"等审批"的细节。问询是尽力而为的：
//! 通道被关闭、`reply` 被丢弃、超时，都会按拒绝（fail-closed）落定。

use agnes_core::message::{ToolArguments, ToolCallId};
use agnes_core::tool::ToolName;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub use crate::ext::ApprovalOutcome;

/// 递给审批界面的一次问询：界面收到后，用 `reply` 送回 true（批准）或
/// false（拒绝）。`reply` 被丢弃等价于拒绝。
#[derive(Debug)]
pub struct ApprovalRequest {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: ToolArguments,
    pub reply: oneshot::Sender<bool>,
}

/// 问询通道：驱动按它发问询；审批界面持有配对的
/// [`mpsc::UnboundedReceiver`]。`Clone` 后可全班共享。
#[derive(Debug, Clone)]
pub struct ApprovalChannel {
    tx: mpsc::UnboundedSender<ApprovalRequest>,
    /// 问询等待答覆的上限；超时按拒绝处理。不设则一直等（或打断）。
    timeout: Option<Duration>,
}

impl ApprovalChannel {
    /// 建一对（驱动发送端，界面接收端）；`timeout` 作用于每次问询。
    pub fn channel(timeout: Option<Duration>) -> (Self, mpsc::UnboundedReceiver<ApprovalRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx, timeout }, rx)
    }

    /// 递送一次问询；接口端已消失则失败（驱动按 fail-closed 落定）。
    pub fn send(
        &self,
        request: ApprovalRequest,
    ) -> Result<(), mpsc::error::SendError<ApprovalRequest>> {
        self.tx.send(request)
    }

    /// 一次问询的等待上限。
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
}

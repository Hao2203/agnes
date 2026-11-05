//! 审批结果词汇：fail-closed 的落定选项。
//!
//! 问询的 IO 与等待在宿主侧（`agnes_agent` 的审批服务）；这里只定义
//! 落定词汇，让 [`HostEvent::ApprovalResolved`](crate::HostEvent) 可以
//! 无歧义地落盘。

/// 审批结果。词汇是 fail-closed 的：只有 [`ApprovalOutcome::AllowedOnce`]
/// 放行；问询被丢弃、超时、运行被打断，都按拒绝落定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// Explicitly approved once.
    AllowedOnce,
    /// Explicitly refused.
    Rejected,
    /// The wait was cancelled (the run was interrupted).
    Cancelled,
    /// No answerer: not wired, channel closed, question dropped, or timed out.
    Unavailable,
}

impl ApprovalOutcome {
    /// The one value that allows the call.
    pub fn allows(self) -> bool {
        matches!(self, Self::AllowedOnce)
    }
}

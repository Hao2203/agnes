//! 驱动状态机的接线与审批落点。

use agnes_io::input::Input as InputPort;
use agnes_io::llm::Llm;
use agnes_io::tool::ToolRegistry;

use crate::approval::ApprovalChannel;
use crate::ext::{ApprovalOutcome, Extensions};
use crate::{RequestBuilder, SessionLog, Sink};

/// 一次驱动的固定接线：全部被动 IO 端与策略注入点。
pub(crate) struct Ports<'a> {
    /// 工具注册表可变：插件可以在下一个模型调用前增删工具。
    pub(crate) registry: &'a mut ToolRegistry,
    pub(crate) llm: &'a dyn Llm,
    pub(crate) input: &'a dyn InputPort,
    pub(crate) requests: &'a dyn RequestBuilder,
    pub(crate) extensions: &'a mut Extensions,
    pub(crate) approvals: Option<&'a ApprovalChannel>,
}

/// 一次 run 的出口：上报与落盘只在这次 run 内借用。
pub(crate) struct Outlets<'s, 'q> {
    pub(crate) sink: &'s mut dyn Sink,
    pub(crate) session: Option<&'q mut dyn SessionLog>,
}

/// 挂起的审批问询如何落定。
pub(super) enum Resolution {
    /// 批准一次 —— 照常执行。
    Allowed,
    /// 拒绝；`outcome` 是 fail-closed 词汇，`detail` 是给模型与界面的理由。
    Denied {
        outcome: ApprovalOutcome,
        detail: &'static str,
    },
    /// 等待被打断 —— 整个 run 收场。
    Interrupted,
}

impl Resolution {
    pub(super) fn outcome(&self) -> ApprovalOutcome {
        match self {
            Resolution::Allowed => ApprovalOutcome::AllowedOnce,
            Resolution::Denied { outcome, .. } => *outcome,
            Resolution::Interrupted => ApprovalOutcome::Cancelled,
        }
    }
}

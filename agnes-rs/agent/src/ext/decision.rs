//! 扩展点的决议词汇：插件只表达「拒绝 / 要审批 / 挡回」，系统再折叠成
//! 可执行决议。

/// 门禁的插件侧结果。
///
/// 没有 `Allow` —— 放行不可表达，因此任何注册顺序都无法把已经发生的
/// [`Verdict::Deny`] 变回放行；链上无人表态时由系统按放行处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 拒绝执行。理由会作为合成错误结果喂回模型；拒绝者的名字由内核盖章。
    Deny { reason: String },
    /// 要求一次人工审批；是否真有审批通道由系统在派发时决定。
    Ask,
}

/// 一次拒绝：谁拒的、为什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    /// 拒绝者（插件名），由内核从 [`Plugin::name`](crate::ext::Plugin::name) 盖章。
    pub by: &'static str,
    /// 理由（随合成结果一起给到模型）。
    pub reason: String,
}

/// 门禁折叠后的系统决议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// 放行执行。
    Allow,
    /// 走审批流。
    Ask,
    /// 拒绝执行。
    Deny(Denied),
}

/// 结果折叠的插件侧结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// 接受原结果。
    Accept,
    /// 挡回：原结果被替换成反馈（仍是一条错误结果入账）。
    Block {
        /// 反馈内容，模型可见。
        feedback: String,
    },
}

/// 挡回后的系统决议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// 挡回者（插件名）。
    pub by: &'static str,
    /// 入账的反馈。
    pub feedback: String,
}

/// 审批结果。fail-closed：只有 [`ApprovalOutcome::AllowedOnce`] 放行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// 明确批准一次。
    AllowedOnce,
    /// 审批者明确拒绝。
    Rejected,
    /// 等待被撤销（运行被打断）。
    Cancelled,
    /// 没有可用的审批者：未接线、通道关闭、答覆消失或超时。
    Unavailable,
}

impl ApprovalOutcome {
    /// 唯一的放行值。
    pub fn allows(self) -> bool {
        matches!(self, Self::AllowedOnce)
    }
}

//! 系统侧的决议词汇：插件只表达意图，系统折叠成可执行结果。

/// 门禁检查点的系统决议。
///
/// 插件侧没有 `Allow`：放行是「无人拒绝且无人要求审批」的系统默认，
/// 因此任何注册顺序都无法把已经发生的 [`Gate::Deny`] 变回放行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// 放行执行。
    Allow,
    /// 走审批流。
    Ask,
    /// 拒绝执行。
    Deny(Denied),
}

/// 一次拒绝：谁拒的、为什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    /// 拒绝者（插件名），由内核从 [`Plugin::name`](crate::ext::Plugin::name) 盖章。
    pub by: &'static str,
    /// 理由（随合成结果一起给到模型）。
    pub reason: String,
}

/// 挡回后的系统决议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// 挡回者（插件名）。
    pub by: &'static str,
    /// 入账的反馈。
    pub feedback: String,
}

/// 一次插件中止：谁中止的、为什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aborted {
    /// 中止者（插件名），由内核盖章。
    pub by: &'static str,
    /// 中止原因（随宿主事件一起落盘/上报）。
    pub reason: String,
}

/// 审批结果。词汇定义在 `agnes_io::session`（它同时是耐久记录的格式），
/// 这里只做再导出，插件与驱动的 import 不变。
pub use agnes_io::session::ApprovalOutcome;

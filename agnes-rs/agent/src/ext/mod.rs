//! 宿主扩展内核：可回收的插件注册 + 事件分发 + 指令收件箱。
//!
//! 设计参照 dsh 的 Cordis 内核，但只取 Rust 里成立的部分：
//!
//! - 插件只有**一个处理入口**（[`Plugin::on_event`]），处理流程与驱动的
//!   具体环节解耦；
//! - 插件返回[`Command`]（意图），内核把它们收进 outbox；执行权在系统，
//!   由驱动在明确的检查点取用；
//! - 门禁用「拒绝单调」的折叠，而不是 `next` 式瀑布 —— 指令里没有
//!   `Allow`，注册顺序无法把拒绝变回放行；
//! - 注册可回收：长期注册拿 [`Registration`] 手柄显式撤销，临时注册用
//!   [`Extensions::scope`]，离开作用域按注册的逆序撤销。

mod bundle;
mod command;
mod decision;
mod outbox;
mod plugin;
mod registry;

pub use bundle::{Bundle, Empty};
pub use command::{Command, QueuedMessage, ToolChange};
pub use decision::{Aborted, ApprovalOutcome, Blocked, Denied, Gate};
pub use plugin::{Plugin, PluginEvent};
pub use registry::{Extensions, Registration, Scope};

#[cfg(test)]
mod tests;

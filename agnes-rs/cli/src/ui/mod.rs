//! 终端后端：渲染命令、状态栏快照，以及 plain/TUI 两个实现。
//!
//! 本模块只管“怎么画”和“怎么读键盘”；实时统计属于 `agnes-agent` 的
//! [`Activity`](agnes_agent::Activity)，由 `render` 投影成 [`Status`]。

use std::time::Duration;

use agnes_core::message::TokenUsage;

pub(crate) mod plain;
pub(crate) mod tui;

/// 渲染线程唯一接收的命令。
pub(crate) enum UiCommand {
    Line(String),
    Prompt(String),
    StreamStart,
    StreamDelta(String),
    StreamEnd,
    CommitAssistant(String),
    StepStats { step: u32, usage: TokenUsage },
    Status(Status),
    Approval(Option<String>),
    Flush,
    Shutdown,
}

/// 状态栏快照，由 render 从 `Activity` 投影而来。
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub model: String,
    pub step: u32,
    pub usage: TokenUsage,
    pub session_usage: TokenUsage,
    pub live_chars: u64,
    pub running: bool,
    pub streaming: bool,
    pub elapsed: Duration,
}

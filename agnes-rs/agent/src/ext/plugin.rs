//! 宿主插件：只处理事件，产生指令；执行权在系统。
//!
//! 插件看不到驱动阶段，也不接触状态机 —— 它收到的是一条统一的事件流
//! （[`PluginEvent`]），返回意图（[`Command`]）。内核把指令收进 outbox，
//! 驱动在明确的检查点取用并执行；core 只看到普通的工具往返，封闭枚举
//! 不因插件扩展。

use agnes_core::message::{ToolCall, ToolOutput};

use super::command::Command;
use super::decision::{ApprovalOutcome, Blocked, Denied};
use crate::record::Event as RecordEvent;

/// 插件看到的统一事件流，按发生顺序。
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum PluginEvent<'a> {
    /// 宿主记录流里的一条，按发生顺序流过。
    Record(&'a RecordEvent),
    /// 一次工具调用即将派发（门禁检查点）。
    ToolCallRequested { call: &'a ToolCall },
    /// 工具已执行、结果尚未入账（结果折叠检查点）。
    ToolCallFinished {
        call: &'a ToolCall,
        output: &'a ToolOutput,
    },
    /// 一次工具调用被门禁拒绝（系统已合成失败结果）。
    ToolCallGated {
        call: &'a ToolCall,
        denied: &'a Denied,
    },
    /// 结果被挡回，入账的是这条反馈。
    ToolResultBlocked {
        call: &'a ToolCall,
        blocked: &'a Blocked,
    },
    /// 系统发出了审批问询。
    ApprovalRequested { call: &'a ToolCall },
    /// 审批落定。
    ApprovalResolved {
        call: &'a ToolCall,
        outcome: ApprovalOutcome,
    },
}

/// 参与 agent 循环的策略层。
pub trait Plugin: Send {
    /// 插件名；也是拒绝、挡回与中止的溯源标签。内核在注册时捕获它，
    /// 因此名字可以是运行期才决定的（运行时安装的插件）。
    fn name(&self) -> &str;

    /// 处理一条事件，返回意图。返回空 `Vec` 表示只观察、不表态。
    fn on_event(&mut self, _event: &PluginEvent<'_>) -> Vec<Command> {
        Vec::new()
    }
}

/// 装箱的插件仍是插件：装配器可以统一按 `Box<dyn Plugin>` 持有。
impl Plugin for Box<dyn Plugin> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        (**self).on_event(event)
    }
}

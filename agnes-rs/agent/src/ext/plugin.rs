//! 宿主插件：类型化的扩展点 —— 每个钩子只在它合法的时机被调用。
//!
//! 插件只表态，执行权在系统：拒绝的合成结果、问询的发出与等待、结果的
//! 替换都由驱动完成；core 只看到普通的工具往返，封闭枚举不因插件扩展。

use agnes_core::event::Event as CoreEvent;
use agnes_core::message::{ToolCall, ToolOutput};

use super::decision::{ApprovalOutcome, Denied, Settlement, Verdict};

/// 只读通知：插件不表态，只观察。
#[derive(Debug, Clone, Copy)]
pub enum Notice<'a> {
    /// core 日志里的一条，按发生顺序流过。
    Core(&'a CoreEvent),
    /// 一次工具调用被门禁拒绝。
    Gated {
        call: &'a ToolCall,
        denied: &'a Denied,
    },
    /// 系统发出了审批问询。
    Asked { call: &'a ToolCall },
    /// 审批落定。
    Decided {
        call: &'a ToolCall,
        outcome: ApprovalOutcome,
    },
    /// 工具结果即将入账（已过结果折叠）。
    Ran {
        call: &'a ToolCall,
        output: &'a ToolOutput,
    },
    /// 结果被挡回，入账的是这条反馈。
    Blocked {
        call: &'a ToolCall,
        by: &'static str,
        feedback: &'a str,
    },
}

/// 参与 agent 循环的策略层。
pub trait Plugin: Send {
    /// 插件名；也是拒绝与挡回的溯源标签。
    fn name(&self) -> &'static str;

    /// 工具调用即将派发。`None` 表示不表态。
    fn gate(&mut self, _call: &ToolCall) -> Option<Verdict> {
        None
    }

    /// 工具结果已产生、尚未入账。
    fn settle(&mut self, _call: &ToolCall, _output: &ToolOutput) -> Settlement {
        Settlement::Accept
    }

    /// 只读观察。
    fn observe(&mut self, _notice: &Notice<'_>) {}
}

/// 装箱的插件仍是插件：装配器可以统一按 `Box<dyn Plugin>` 持有。
impl Plugin for Box<dyn Plugin> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn gate(&mut self, call: &ToolCall) -> Option<Verdict> {
        (**self).gate(call)
    }

    fn settle(&mut self, call: &ToolCall, output: &ToolOutput) -> Settlement {
        (**self).settle(call, output)
    }

    fn observe(&mut self, notice: &Notice<'_>) {
        (**self).observe(notice);
    }
}

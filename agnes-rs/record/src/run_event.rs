//! 运行期上报流：run 级生命周期事件、日志镜像与宿主语义。
//!
//! 与 [`crate::event::Event`]（持久化记录）不同，[`RunEvent`] 是观察者
//! 词汇：驱动/系统把它按发生顺序发出，渲染与落盘各自按需消费。core 的
//! 封闭枚举不因插件扩展 —— 插件/系统的语义都长在 [`RunEventKind`] 上。
//!
//! [`RunEvent`] 是信封：`agent` 是事件所属 agent 实体，消费方（会话
//! 落盘、活动快照、渲染）按它路由，一个世界里的多个 agent 各看各的流；
//! `kind` 是事件本体。

use crate::approval::ApprovalOutcome;
use crate::decision::Denied;
use crate::event::Event as RecordEvent;
use crate::origin::Origin;
use agnes_core::content::Content;
use agnes_core::message::{ToolCall, ToolCallId};
use agnes_core::tool::ToolName;
use agnes_core::turn::Outcome;
use agnes_ecs::Entity;
use jiff::Timestamp;

/// 一条运行期上报：所属 agent 实体加事件本体。
#[derive(Debug, Clone)]
pub struct RunEvent {
    /// 事件所属的 agent 实体：消费方按它把事件路由回所属会话。
    pub agent: Entity,
    /// 事件本体。
    pub kind: RunEventKind,
}

/// 运行期间的上报本体：run 级生命周期事件、日志的镜像事件，以及
/// 插件语义的宿主事件 —— 插件的面貌只长在这里，core 不见。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RunEventKind {
    Started {
        at: Timestamp,
        /// 本轮的开局目标；无 goal 启动时为 `None`。
        goal: Option<Content>,
        tools: Vec<ToolName>,
    },
    /// Agent 循环里发生的一条记录事件 —— turn/step 生命周期或消息；
    /// 已把它记入日志，再按发生顺序转发。
    Recorded {
        at: Timestamp,
        event: RecordEvent,
    },
    /// 门禁拒绝了一次工具调用；机器将收到一条合成的失败结果。
    ToolGated {
        at: Timestamp,
        call: ToolCall,
        /// 拒绝者与理由（理由随合成结果一起给到模型）。
        denied: Denied,
    },
    /// 权限门禁发起的审批问询；随即挂起，等待答覆或打断。
    ApprovalRequested {
        at: Timestamp,
        call: ToolCall,
    },
    /// 一问一答落定：只有 [`ApprovalOutcome::AllowedOnce`] 照常执行，其余
    /// （显式拒绝、超时、答覆消失、打断）都以合成失败结果推进。
    ApprovalResolved {
        at: Timestamp,
        call_id: ToolCallId,
        outcome: ApprovalOutcome,
    },
    /// 插件中止了 run；turn 以 [`Outcome::Interrupted`] 收场，原因在这里。
    Aborted {
        at: Timestamp,
        by: Origin,
        reason: String,
    },
    Finished {
        at: Timestamp,
        outcome: Outcome,
    },
    /// The run aborted with an error; emitted before the error is returned so
    /// observers always see a terminal event.
    Failed {
        at: Timestamp,
        cause: String,
    },
}

/// 接收 [`RunEvent`]s。任何 `FnMut(&RunEvent)` 都是个 `Sink`。
pub trait Sink {
    fn event(&mut self, event: &RunEvent);
}

impl<F: FnMut(&RunEvent)> Sink for F {
    fn event(&mut self, event: &RunEvent) {
        self(event)
    }
}

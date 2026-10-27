//! 驱动器：宿主状态机（私有模块 `driver`）解释 [`agnes_core::turn`] 状态机
//! 产生的动作 —— 就地处理动作、管理等待点与输入输出。
//!
//! 机器是类型状态的（阶段即类型）；这个 crate 做 IO —— 通过 `agnes-io`
//! 的异步接缝 —— 把结果经对应阶段的方法喂回机器，一路用 [`Event`] 上报
//! 发生的事。运行时（tokio、smol、…）由宿主的执行器与传输层选择，与本
//! crate 无关。
//!
//! 宿主用 [`Host`] 装配：模型、输入、工具、扩展内核、审批口与会话日志在
//! 装载期一次性解析。驱动从不自己组装提示词：[`RequestBuilder`] 是宿主
//! 的转换者，在每次模型调用前由它把日志变成请求。
//!
//! 工具执行前先过扩展内核（[`ext`]）：插件只处理事件、返回指令
//! （[`ext::Command`]），执行权在系统 —— 放行执行、拒绝则以合成失败结果
//! 推进机器（core 日志只是个普通失败工具）、要审批则走系统自有的审批
//! 服务（[`approval`]）挂起等待答覆或打断。注入进队列、工具变更在下一个
//! 模型调用前落地，都由驱动在检查点执行。core 的封闭枚举不因插件扩展 ——
//! 插件的语义都发在宿主的 [`Event`] 变体上。

mod driver;
mod host;
mod log;

pub mod approval;
pub mod ext;
pub mod permission;
pub mod tools;

pub use host::{Host, HostBuilder, LoadError, RunRequest};
pub use log::Log;

use agnes_core::config::ModelName;
use agnes_core::content::Content;
use agnes_core::event::Event as CoreEvent;
use agnes_core::llm::{LlmRequest, Prompt};
use agnes_core::message::{AssistantMessage, Message, TokenUsage, ToolCall, ToolCallId};
use agnes_core::tool::ToolName;
use agnes_core::turn::Outcome;
use agnes_io::llm::LlmError;
use jiff::Timestamp;

use approval::ApprovalOutcome;
use ext::{Aborted, Denied};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    /// 会话日志写失败：宁可中止，也不在没有审计轨迹的情况下继续跑。
    #[error(transparent)]
    Session(#[from] agnes_io::session::SessionError),
    /// 驱动内部不变量被破坏（不是用户输入或 IO 错误）。
    #[error("driver invariant violated: {reason}")]
    Invariant { reason: &'static str },
    /// 原始错误保留为 source；审计写盘失败单独可见，不再被吞掉。
    #[error("run aborted; the audit trail could not be written: {audit}")]
    Audit {
        #[source]
        cause: Box<AgentError>,
        audit: agnes_io::session::SessionError,
    },
}

/// 把会话日志转换成具体请求。宿主提供一个，驱动在每次模型调用前询问它 ——
/// 提示词长什么样完全是宿主的事，[`Prompt::from_events`] 只是标准投影。
pub trait RequestBuilder {
    fn build(&self, log: &Log) -> LlmRequest;
}

/// 标准转换者：日志的标准投影（[`Prompt::from_events`]）加模型名。
/// 对提示词有想法的宿主自己实现 [`RequestBuilder`]。
#[derive(Debug, Clone)]
pub struct StandardRequestBuilder {
    model: ModelName,
}

impl StandardRequestBuilder {
    pub fn new(model: ModelName) -> Self {
        Self { model }
    }
}

impl RequestBuilder for StandardRequestBuilder {
    fn build(&self, log: &Log) -> LlmRequest {
        LlmRequest {
            model: self.model.clone(),
            prompt: Prompt::from_events(log.events()),
        }
    }
}

/// 运行期间的上报：run 级生命周期事件、日志的镜像事件，以及
/// 插件语义的宿主事件 —— 插件的面貌只长在这里，core 不见。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    Started {
        at: Timestamp,
        goal: Content,
        tools: Vec<ToolName>,
    },
    /// Agent 循环里发生的一条核心事件 —— turn/step 生命周期或消息；
    /// 驱动已把它记入日志，再按发生顺序转发。
    Recorded {
        at: Timestamp,
        event: CoreEvent,
    },
    /// 扩展内核拒绝了一次工具调用；机器将收到一条合成的失败结果。
    ToolGated {
        at: Timestamp,
        call: ToolCall,
        /// 拒绝者与理由（理由随合成结果一起给到模型）。
        denied: Denied,
    },
    /// 权限插件发起的审批问询；驱动随即挂起，等待答覆或打断。
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
        by: &'static str,
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
        error: String,
    },
}

/// 接收 [`Event`]s。任何 `FnMut(&Event)` 都是个 `Sink`。
pub trait Sink {
    fn event(&mut self, event: &Event);
}

impl<F: FnMut(&Event)> Sink for F {
    fn event(&mut self, event: &Event) {
        self(event)
    }
}

/// 一次完整 turn 的结果。
///
/// `log` 是驱动记录下来的会话日志（系统提示与工具环境也在其中，作为
/// 事件）；作为下一次 [`Host::run`] 的 `log` 参数即可延续会话。
/// `pending_messages` 是没来得及生效的注入消息（预算耗尽、最后一次答复
/// 时仍没轮到，或插件在收场后才注入），宿主接着用。
/// `aborted` 是插件中止的原因；此时 `outcome` 仍是
/// [`Outcome::Interrupted`]，core 词汇不因插件扩展。
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub outcome: Outcome,
    pub steps_used: u32,
    pub usage: TokenUsage,
    pub log: Log,
    pub pending_messages: Vec<Message>,
    pub aborted: Option<Aborted>,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
}

impl RunSummary {
    pub fn duration(&self) -> jiff::SignedDuration {
        self.finished_at.duration_since(self.started_at)
    }

    /// The last assistant text in the log — the final answer when the
    /// outcome is [`Outcome::Answered`].
    pub fn reply(&self) -> Option<&Content> {
        self.log
            .messages()
            .filter_map(|message| match message {
                Message::Assistant(AssistantMessage::Text(content)) => Some(content),
                _ => None,
            })
            .last()
    }
}

#[cfg(test)]
mod tests;

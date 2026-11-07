//! 基于 ECS 的 agent 运行时：core 的 typestate 是唯一真相，本 crate 的
//! 系统解释阶段提示、派发工具、按 [`agnes_record`] 的词汇记录事件。
//!
//! 模型调用不做 I/O：运行时写 `LlmCall` / `LlmCancel` 消息、读 `LlmReply`
//! 消息（定义在 `agnes_io::llm`），provider 插件负责编解码与网络往返。
//!
//! 入口是 [`runtime::AgentPlugin`]：注册后宿主经 `RuntimeSender` 注入
//! turn 请求 / 用户消息 / 审批答覆 / 打断，收场结果经
//! [`runtime::TurnFinished`] 消息交还。工具表用 [`runtime::ToolsPlugin`]
//! 注册；权限规则用 [`permission::PermissionRules`]；审批问询通道用
//! [`approval::ApprovalChannel`]。
//!
//! 异步任务由宿主安装的 `agnes_plugin_async::TokioPlugin`（或
//! `ExecutorPlugin`）提供，运行时选择与本 crate 无关。

pub mod activity;
pub mod approval;
mod core;
pub mod model;
pub mod permission;

pub mod runtime;
pub mod tools;

pub use activity::{Activity, ActivityPlugin, Phase};
pub use approval::{ApprovalChannel, ApprovalRequest};
pub use core::{AgentCore, Inbox};
pub use model::{ModelPlugin, ModelSettings, ModelSettingsChanged, SetModel, SetThinking};
pub use permission::{PermissionRules, Rule, RuleError};
pub use runtime::{
    ActiveTurn, AgentPlugin, ApprovalDecided, InterruptTurn, RequestBuilderResource, ToolFinished,
    ToolsPlugin, TurnFailure, TurnFailureKind, TurnFinished, TurnOwner, TurnRequested, UserMessage,
};

use agnes_core::llm::LlmRequest;
use agnes_core::turn::Turn;

/// 把 [`Turn`] 转换成具体请求。宿主提供一个，运行时在每次模型调用前
/// 询问它 —— 提示词长什么样完全是宿主的事，[`Turn::prompt`] 只是标准
/// 投影；模型与思考参数来自当前的 [`ModelSettings`]。
pub trait RequestBuilder: Send + Sync {
    fn build(&self, turn: &Turn, settings: &ModelSettings) -> LlmRequest;
}

/// 标准转换者：turn 的标准投影（[`Turn::prompt`]）加当前请求设置。
/// 对提示词有想法的宿主自己实现 [`RequestBuilder`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct StandardRequestBuilder;

impl RequestBuilder for StandardRequestBuilder {
    fn build(&self, turn: &Turn, settings: &ModelSettings) -> LlmRequest {
        LlmRequest {
            model: settings.model().clone(),
            thinking: settings.thinking(),
            prompt: turn.prompt(),
        }
    }
}

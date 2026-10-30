//! 插件指令：插件只表达意图，系统在检查点执行。
//!
//! 指令自带目标 `ToolCallId`；内核按发布顺序收进内部收件箱，系统只在
//! 对应调用的检查点取用。没有 `Allow` —— 拒绝单调，任何注册顺序都无法
//! 把已经发生的拒绝变回放行。

use std::fmt;
use std::sync::Arc;

use agnes_core::message::{Message, ToolCallId};
use agnes_core::tool::ToolName;
use agnes_io::tool::{Tool, ToolRegistry};

use super::origin::Origin;

/// 插件对一次事件表达的意图。
///
/// `#[non_exhaustive]`：新增指令是内核的增量，插件的匹配需要 `_`。
#[non_exhaustive]
pub enum Command {
    /// 拒绝一次调用。理由会作为合成错误结果喂回模型。
    Deny { call: ToolCallId, reason: String },
    /// 要求一次人工审批；是否真的问询由系统在派发时决定。
    RequireApproval { call: ToolCallId },
    /// 挡回一次工具结果：入账的是反馈（仍是一条错误结果）。
    ReplaceResult { call: ToolCallId, feedback: String },
    /// 向会话注入一条消息（任意角色）；由系统放进注入队列，在 step 边界
    /// 生效，模型看到的角色与这里一致。
    InjectMessage { message: Message },
    /// 中止整个 run。收场复用 core 的 `interrupted`。
    Abort { reason: String },
    /// 运行期注入一个工具；系统在下一个模型调用前应用并记录新 header。
    AddTool { tool: Arc<dyn Tool> },
    /// 运行期移除一个工具；不存在的名字是无操作。
    RemoveTool { name: ToolName },
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deny { call, reason } => f
                .debug_struct("Deny")
                .field("call", call)
                .field("reason", reason)
                .finish(),
            Self::RequireApproval { call } => f
                .debug_struct("RequireApproval")
                .field("call", call)
                .finish(),
            Self::ReplaceResult { call, feedback } => f
                .debug_struct("ReplaceResult")
                .field("call", call)
                .field("feedback", feedback)
                .finish(),
            Self::InjectMessage { message } => f
                .debug_struct("InjectMessage")
                .field("message", message)
                .finish(),
            Self::Abort { reason } => f.debug_struct("Abort").field("reason", reason).finish(),
            Self::AddTool { tool } => f
                .debug_struct("AddTool")
                .field("tool", tool.spec().name())
                .finish(),
            Self::RemoveTool { name } => f.debug_struct("RemoveTool").field("name", name).finish(),
        }
    }
}

/// 注入队列里的一条消息。
#[derive(Debug)]
pub struct QueuedMessage {
    by: Origin,
    message: Message,
}

impl QueuedMessage {
    /// 内核入队时使用；插件只返回 [`Command::InjectMessage`]。
    pub(super) fn new(by: Origin, message: Message) -> Self {
        Self { by, message }
    }

    /// 入队的插件名（溯源）。
    pub fn by(&self) -> &Origin {
        &self.by
    }

    /// 待注入的消息。
    pub fn message(&self) -> &Message {
        &self.message
    }

    /// 取走消息。
    pub fn into_message(self) -> Message {
        self.message
    }
}

/// 一次运行期工具变更。
pub enum ToolChange {
    /// 注入一个工具；重名时保留既有工具（不 shadow）。
    Add(Arc<dyn Tool>),
    /// 移除一个工具；名字不存在时是 no-op。
    Remove(ToolName),
}

impl ToolChange {
    /// 应用一次变更，返回注册表是否真的变了。
    ///
    /// 重名 `Add` 保留既有工具、缺失的 `Remove` 是 no-op；两者都返回
    /// `false`，调用方据此避免记录无意义的 header。
    pub(crate) fn apply(self, registry: &mut ToolRegistry) -> bool {
        match self {
            Self::Add(tool) => registry.register(tool).is_ok(),
            Self::Remove(name) => registry.deregister(&name).is_some(),
        }
    }
}

impl fmt::Debug for ToolChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add(tool) => f.debug_tuple("Add").field(tool.spec().name()).finish(),
            Self::Remove(name) => f.debug_tuple("Remove").field(name).finish(),
        }
    }
}

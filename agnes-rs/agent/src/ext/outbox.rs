//! 指令收件箱：插件发布事件后，指令在这里等待系统在检查点执行。
//!
//! 三个队列分工：
//!
//! - `pending`：定向决议（拒绝 / 审批 / 结果替换 / 中止），按检查点取用；
//! - `injections`：待注入消息，FIFO，驱动在待调用阶段排空并立即并入会话；
//! - `tools`：运行期工具变更，按发布顺序应用。

use std::collections::VecDeque;

use agnes_core::message::ToolCallId;

use super::command::{Command, QueuedMessage, ToolChange};
use super::decision::{Aborted, Blocked, Denied, Gate};

/// 一条带来源的定向指令；来源由内核盖章，插件无法伪造。
struct Issued {
    by: &'static str,
    command: Command,
}

/// 指令收件箱：插件发布事件后，指令在这里等待系统在检查点执行。
#[derive(Default)]
pub(super) struct Outbox {
    pending: Vec<Issued>,
    injections: VecDeque<QueuedMessage>,
    tools: Vec<ToolChange>,
}

impl Outbox {
    /// 收下某插件对一次事件返回的全部指令。
    pub(super) fn issue(&mut self, by: &'static str, commands: Vec<Command>) {
        for command in commands {
            match command {
                Command::InjectMessage { content } => {
                    self.injections.push_back(QueuedMessage::new(by, content));
                }
                Command::AddTool { tool } => self.tools.push(ToolChange::Add(tool)),
                Command::RemoveTool { name } => self.tools.push(ToolChange::Remove(name)),
                command => self.pending.push(Issued { by, command }),
            }
        }
    }

    /// 取走所有匹配的指令，其余保持原顺序。
    fn take_matching(&mut self, mut predicate: impl FnMut(&Command) -> bool) -> Vec<Issued> {
        let pending = std::mem::take(&mut self.pending);
        let mut matched = Vec::new();
        let mut rest = Vec::with_capacity(pending.len());
        for issued in pending {
            if predicate(&issued.command) {
                matched.push(issued);
            } else {
                rest.push(issued);
            }
        }
        self.pending = rest;
        matched
    }

    /// 门禁检查点：取走针对该调用的拒绝/审批指令，`Deny > Ask > Allow`。
    /// 多条拒绝取注册顺序第一条。
    pub(super) fn gate(&mut self, call: &ToolCallId) -> Gate {
        let matched = self.take_matching(|command| {
            matches!(
                command,
                Command::Deny {
                    call: target,
                    ..
                } | Command::RequireApproval { call: target }
                    if target == call
            )
        });
        let mut denied: Option<(&'static str, String)> = None;
        let mut ask = false;
        for issued in matched {
            match issued.command {
                Command::Deny { reason, .. } => {
                    if denied.is_none() {
                        denied = Some((issued.by, reason));
                    }
                }
                Command::RequireApproval { .. } => ask = true,
                _ => {}
            }
        }
        match denied {
            Some((by, reason)) => Gate::Deny(Denied { by, reason }),
            None if ask => Gate::Ask,
            None => Gate::Allow,
        }
    }

    /// 结果检查点：取走针对该调用的结果替换，第一条生效。
    pub(super) fn replacement(&mut self, call: &ToolCallId) -> Option<Blocked> {
        self.take_matching(|command| {
            matches!(
                command,
                Command::ReplaceResult {
                    call: target,
                    ..
                } if target == call
            )
        })
        .into_iter()
        .find_map(|issued| match issued.command {
            Command::ReplaceResult { feedback, .. } => Some(Blocked {
                by: issued.by,
                feedback,
            }),
            _ => None,
        })
    }

    /// 取走第一条中止指令。
    pub(super) fn abort(&mut self) -> Option<Aborted> {
        self.take_matching(|command| matches!(command, Command::Abort { .. }))
            .into_iter()
            .find_map(|issued| match issued.command {
                Command::Abort { reason } => Some(Aborted {
                    by: issued.by,
                    reason,
                }),
                _ => None,
            })
    }

    /// 取走队首的注入消息。
    pub(super) fn take_injection(&mut self) -> Option<QueuedMessage> {
        self.injections.pop_front()
    }

    /// 取走全部剩余注入（收场时交还宿主）。
    pub(super) fn take_injections(&mut self) -> Vec<QueuedMessage> {
        self.injections.drain(..).collect()
    }

    /// 清空上次运行遗留的注入。
    pub(super) fn clear_injections(&mut self) {
        self.injections.clear();
    }

    /// 取走全部工具变更，保持发布顺序。
    pub(super) fn take_tool_changes(&mut self) -> Vec<ToolChange> {
        std::mem::take(&mut self.tools)
    }

    /// 是否有待执行的工具变更。
    pub(super) fn has_tool_changes(&self) -> bool {
        !self.tools.is_empty()
    }

    /// 丢弃收场后不可能再执行的定向指令。
    pub(super) fn clear_decisions(&mut self) {
        self.pending.clear();
    }
}

//! 权限插件：规则表把"工具调用请求"事件映射成指令，不由它执行。
//!
//! 规则优先级 deny > ask > 放行（无规则默认不表态，交给后续插件；链上
//! 无人表态则系统默认放行）。deny 命中发 [`Command::Deny`]；ask 命中发
//! [`Command::Ask`] —— 问询的构造、挂起、超时与答覆落定全部由系统
//! （[`crate::approval`]）完成，本插件只看事件、发指令。

use agnes_core::tool::ToolName;

use crate::plugin::{Command, Plugin, PluginEvent};

/// 一条工具名匹配规则：精确或前缀。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// 工具名精确匹配。
    Name(ToolName),
    /// 工具名以此开头。
    Prefix(String),
}

impl Rule {
    pub fn matches(&self, tool_name: &ToolName) -> bool {
        match self {
            Rule::Name(name) => name == tool_name,
            Rule::Prefix(prefix) => tool_name.as_str().starts_with(prefix),
        }
    }
}

/// 基于规则表的权限插件。
#[derive(Debug, Default)]
#[must_use]
pub struct PermissionPlugin {
    deny: Vec<Rule>,
    ask: Vec<Rule>,
}

impl PermissionPlugin {
    /// 新插件：`deny` 一律拒绝（附理由）；`ask` 命中走审批指令；其余
    /// 不表态。
    pub fn new(deny: Vec<Rule>, ask: Vec<Rule>) -> Self {
        Self { deny, ask }
    }
}

impl Plugin for PermissionPlugin {
    fn name(&self) -> &'static str {
        "permission"
    }

    fn on_event(&mut self, event: &PluginEvent) -> Option<Command> {
        let PluginEvent::ToolCallRequested { call } = event else {
            return None;
        };
        if self.deny.iter().any(|rule| rule.matches(&call.name)) {
            return Some(Command::Deny(format!(
                "tool `{}` is denied by policy",
                call.name
            )));
        }
        if self.ask.iter().any(|rule| rule.matches(&call.name)) {
            return Some(Command::Ask);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::message::{ToolArguments, ToolCall, ToolCallId};
    use std::str::FromStr;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c1"),
            name: ToolName::from_str(name).unwrap(),
            arguments: ToolArguments::new("{}"),
        }
    }

    #[test]
    fn rules_map_to_commands_in_priority_order() {
        let mut plugin = PermissionPlugin::new(
            vec![Rule::Prefix("shell".into())],
            vec![Rule::Name(ToolName::from_str("edit").unwrap())],
        );

        // deny 前缀命中 → 拒绝指令
        let issued = plugin.on_event(&PluginEvent::ToolCallRequested {
            call: call("shell_find"),
        });
        assert_eq!(
            issued,
            Some(Command::Deny("tool `shell_find` is denied by policy".into()))
        );

        // ask 命中 → 审批指令（是否接线由系统执行时判断）
        let issued = plugin.on_event(&PluginEvent::ToolCallRequested { call: call("edit") });
        assert_eq!(issued, Some(Command::Ask));

        // 无规则 → 不表态
        let issued = plugin.on_event(&PluginEvent::ToolCallRequested { call: call("note") });
        assert_eq!(issued, None);
    }

    #[test]
    fn deny_wins_over_ask_when_both_match() {
        let mut plugin = PermissionPlugin::new(
            vec![Rule::Name(ToolName::from_str("run_shell").unwrap())],
            vec![Rule::Prefix("run_".into())],
        );

        // 名字同时命中 deny（精确）与 ask（前缀）：deny 先定案
        let issued = plugin.on_event(&PluginEvent::ToolCallRequested {
            call: call("run_shell"),
        });
        assert!(matches!(issued, Some(Command::Deny(_))));

        // 仅 ask 前缀命中：走审批指令
        let issued = plugin.on_event(&PluginEvent::ToolCallRequested {
            call: call("run_edit"),
        });
        assert_eq!(issued, Some(Command::Ask));
    }

    #[test]
    fn non_request_events_never_elicit_commands() {
        let mut plugin = PermissionPlugin::new(
            vec![Rule::Prefix("x".into())],
            vec![],
        );
        assert_eq!(
            plugin.on_event(&PluginEvent::ApprovalAnswered {
                call: call("x"),
                approved: false,
            }),
            None
        );
    }
}

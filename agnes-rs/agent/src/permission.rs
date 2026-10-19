//! 权限插件：规则表把「工具调用」映射成门禁表态。
//!
//! 规则优先级 deny > ask > 不表态。deny 命中返回 [`Verdict::Deny`]；ask
//! 命中返回 [`Verdict::Ask`] —— 问询的构造、挂起、超时与答覆落定全部由
//! 系统（[`crate::approval`]）完成，本插件只看调用、发表态。
//!
//! 门禁折叠是拒绝单调的：本插件即使只是不表态，也不会削弱其他插件的
//! 拒绝。

use agnes_core::message::ToolCall;
use agnes_core::tool::ToolName;
use std::fmt;

use crate::ext::{Bundle, Plugin, Verdict};
use crate::host::{Host, LoadError};

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

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rule::Name(name) => write!(f, "`{name}`"),
            Rule::Prefix(prefix) => write!(f, "`{prefix}*`"),
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
    /// 新插件：`deny` 一律拒绝（附理由）；`ask` 命中走审批；其余不表态。
    pub fn new(deny: Vec<Rule>, ask: Vec<Rule>) -> Self {
        Self { deny, ask }
    }
}

impl Plugin for PermissionPlugin {
    fn name(&self) -> &'static str {
        "permission"
    }

    fn gate(&mut self, call: &ToolCall) -> Option<Verdict> {
        if self.deny.iter().any(|rule| rule.matches(&call.name)) {
            return Some(Verdict::Deny {
                reason: format!("tool `{}` is denied by policy", call.name),
            });
        }
        if self.ask.iter().any(|rule| rule.matches(&call.name)) {
            return Some(Verdict::Ask);
        }
        None
    }
}

/// 权限装载单元：先校验规则，再注册 [`PermissionPlugin`]。
///
/// 校验发生在装载期而不是第一次调用时：规则必须能命中至少一个已注册
/// 的工具（否则它永远不会生效）；配了 ask 就必须接通审批口（否则每次
/// 命中都只会 fail-closed 拒绝）。
pub struct PolicyBundle {
    deny: Vec<Rule>,
    ask: Vec<Rule>,
}

impl PolicyBundle {
    pub fn new(deny: Vec<Rule>, ask: Vec<Rule>) -> Self {
        Self { deny, ask }
    }
}

impl Bundle for PolicyBundle {
    fn name(&self) -> &'static str {
        "permission"
    }

    fn install(&self, host: &mut Host<'_>) -> Result<(), LoadError> {
        for rule in self.deny.iter().chain(&self.ask) {
            if !host.tools().names().any(|name| rule.matches(name)) {
                return Err(LoadError::UnknownTool {
                    bundle: self.name(),
                    rule: rule.to_string(),
                });
            }
        }
        if !self.ask.is_empty() && !host.has_approvals() {
            return Err(LoadError::AskWithoutApprovals {
                bundle: self.name(),
            });
        }
        host.extensions_mut()
            .register(PermissionPlugin::new(self.deny.clone(), self.ask.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agnes_core::message::{ToolArguments, ToolCallId};
    use std::str::FromStr;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c1"),
            name: ToolName::from_str(name).unwrap(),
            arguments: ToolArguments::new("{}"),
        }
    }

    #[test]
    fn rules_map_to_verdicts_in_priority_order() {
        let mut plugin = PermissionPlugin::new(
            vec![Rule::Prefix("shell".into())],
            vec![Rule::Name(ToolName::from_str("edit").unwrap())],
        );

        // deny 前缀命中 → 拒绝
        assert_eq!(
            plugin.gate(&call("shell_find")),
            Some(Verdict::Deny {
                reason: "tool `shell_find` is denied by policy".into()
            })
        );

        // ask 命中 → 审批（是否接线由系统执行时判断）
        assert_eq!(plugin.gate(&call("edit")), Some(Verdict::Ask));

        // 无规则 → 不表态
        assert_eq!(plugin.gate(&call("note")), None);
    }

    #[test]
    fn deny_wins_over_ask_when_both_match() {
        let mut plugin = PermissionPlugin::new(
            vec![Rule::Name(ToolName::from_str("run_shell").unwrap())],
            vec![Rule::Prefix("run_".into())],
        );

        // 名字同时命中 deny（精确）与 ask（前缀）：deny 先定案
        assert!(matches!(
            plugin.gate(&call("run_shell")),
            Some(Verdict::Deny { .. })
        ));

        // 仅 ask 前缀命中：走审批
        assert_eq!(plugin.gate(&call("run_edit")), Some(Verdict::Ask));
    }
}

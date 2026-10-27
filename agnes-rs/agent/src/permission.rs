//! 权限插件：规则表把「工具调用」映射成指令。
//!
//! 规则优先级 deny > ask > 不表态。deny 命中返回 [`Command::Deny`]；ask
//! 命中返回 [`Command::RequireApproval`] —— 问询的构造、挂起、超时与
//! 答覆落定全部由系统（[`crate::approval`]）完成，本插件只看事件、
//! 发指令。
//!
//! 门禁折叠是拒绝单调的：本插件即使只是不表态，也不会削弱其他插件的
//! 拒绝。

use agnes_core::config::ConfigError;
use agnes_core::tool::ToolName;
use std::fmt;
use std::str::FromStr;

use crate::ext::{Bundle, Command, Plugin, PluginEvent};
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

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuleError {
    #[error("empty prefix in `{spec}`")]
    EmptyPrefix { spec: String },
    #[error(transparent)]
    InvalidName(#[from] ConfigError),
}

impl FromStr for Rule {
    type Err = RuleError;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        match spec.strip_suffix('*') {
            Some(prefix) if !prefix.is_empty() => Ok(Rule::Prefix(prefix.to_owned())),
            Some(_) => Err(RuleError::EmptyPrefix {
                spec: spec.to_owned(),
            }),
            None => Ok(Rule::Name(ToolName::from_str(spec)?)),
        }
    }
}

/// 权限规则表：把两个同类型列表收进一个结构，避免位置参数写反。
#[must_use = "pass the rules to PermissionPlugin or PolicyBundle"]
#[derive(Debug, Clone, Default)]
pub struct PermissionRules {
    pub deny: Vec<Rule>,
    pub ask: Vec<Rule>,
}

/// 基于规则表的权限插件。
#[derive(Debug)]
pub struct PermissionPlugin {
    rules: PermissionRules,
}

impl PermissionPlugin {
    /// 新插件：`deny` 一律拒绝（附理由）；`ask` 命中走审批；其余不表态。
    pub fn new(rules: PermissionRules) -> Self {
        Self { rules }
    }
}

impl Plugin for PermissionPlugin {
    fn name(&self) -> &'static str {
        "permission"
    }

    fn on_event(&mut self, event: &PluginEvent<'_>) -> Vec<Command> {
        let PluginEvent::ToolCallRequested { call } = event else {
            return Vec::new();
        };
        if self.rules.deny.iter().any(|rule| rule.matches(&call.name)) {
            return vec![Command::Deny {
                call: call.id.clone(),
                reason: format!("tool `{}` is denied by policy", call.name),
            }];
        }
        if self.rules.ask.iter().any(|rule| rule.matches(&call.name)) {
            return vec![Command::RequireApproval {
                call: call.id.clone(),
            }];
        }
        Vec::new()
    }
}

/// 权限装载单元：先校验规则，再注册 [`PermissionPlugin`]。
///
/// 校验发生在装载期而不是第一次调用时：规则必须能命中至少一个已注册
/// 的工具（否则它永远不会生效）；配了 ask 就必须接通审批口（否则每次
/// 命中都只会 fail-closed 拒绝）。
///
/// 运行期会由插件动态注入工具时，用 [`PolicyBundle::allow_unresolved`]
/// 跳过第一条检查（规则可以指向尚未出现的工具）。
#[must_use = "install the policy bundle into a host"]
pub struct PolicyBundle {
    rules: PermissionRules,
    allow_unresolved: bool,
}

impl PolicyBundle {
    pub fn new(rules: PermissionRules) -> Self {
        Self {
            rules,
            allow_unresolved: false,
        }
    }

    /// 允许规则暂时不命中任何已注册工具：用于规则指向运行期才注入的
    /// 工具。默认仍是 fail-loud。
    pub fn allow_unresolved(mut self) -> Self {
        self.allow_unresolved = true;
        self
    }
}

impl Bundle for PolicyBundle {
    fn name(&self) -> &'static str {
        "permission"
    }

    fn install(&self, host: &mut Host<'_>) -> Result<(), LoadError> {
        if !self.allow_unresolved {
            for rule in self.rules.deny.iter().chain(&self.rules.ask) {
                if !host.tools().names().any(|name| rule.matches(name)) {
                    return Err(LoadError::UnknownTool {
                        bundle: self.name(),
                        rule: rule.to_string(),
                    });
                }
            }
        }
        if !self.rules.ask.is_empty() && !host.has_approvals() {
            return Err(LoadError::AskWithoutApprovals {
                bundle: self.name(),
            });
        }
        host.extensions_mut()
            .register(PermissionPlugin::new(self.rules.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext::PluginEvent;
    use agnes_core::message::{ToolArguments, ToolCall, ToolCallId};
    use std::str::FromStr;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c1"),
            name: ToolName::from_str(name).unwrap(),
            arguments: ToolArguments::new("{}"),
        }
    }

    fn commands(plugin: &mut PermissionPlugin, call: &ToolCall) -> Vec<Command> {
        plugin.on_event(&PluginEvent::ToolCallRequested { call })
    }

    #[test]
    fn rules_map_to_commands_in_priority_order() {
        let mut plugin = PermissionPlugin::new(PermissionRules {
            deny: vec![Rule::Prefix("shell".into())],
            ask: vec![Rule::Name(ToolName::from_str("edit").unwrap())],
        });

        // deny 前缀命中 → 拒绝
        let denied = commands(&mut plugin, &call("shell_find"));
        assert!(matches!(
            denied.as_slice(),
            [Command::Deny { call: id, reason }]
                if id.as_str() == "c1" && reason == "tool `shell_find` is denied by policy"
        ));

        // ask 命中 → 审批（是否接线由系统执行时判断）
        let asked = commands(&mut plugin, &call("edit"));
        assert!(matches!(
            asked.as_slice(),
            [Command::RequireApproval { call: id }] if id.as_str() == "c1"
        ));

        // 无规则 → 不表态
        assert!(commands(&mut plugin, &call("note")).is_empty());
    }

    #[test]
    fn deny_wins_over_ask_when_both_match() {
        let mut plugin = PermissionPlugin::new(PermissionRules {
            deny: vec![Rule::Name(ToolName::from_str("run_shell").unwrap())],
            ask: vec![Rule::Prefix("run_".into())],
        });

        // 名字同时命中 deny（精确）与 ask（前缀）：deny 先定案
        assert!(matches!(
            commands(&mut plugin, &call("run_shell")).as_slice(),
            [Command::Deny { .. }]
        ));

        // 仅 ask 前缀命中：走审批
        assert!(matches!(
            commands(&mut plugin, &call("run_edit")).as_slice(),
            [Command::RequireApproval { .. }]
        ));
    }

    #[test]
    fn a_non_request_event_produces_nothing() {
        let mut plugin = PermissionPlugin::new(PermissionRules {
            deny: vec![Rule::Prefix("shell".into())],
            ask: vec![],
        });
        let call = call("shell_find");
        assert!(
            plugin
                .on_event(&PluginEvent::ToolCallFinished {
                    call: &call,
                    output: &agnes_core::message::ToolOutput::success("ok"),
                })
                .is_empty()
        );
    }
}

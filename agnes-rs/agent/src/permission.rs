//! 权限规则：把「工具名」映射成 deny / ask / 不表态。
//!
//! 规则优先级 deny > ask > 不表态。门禁在运行时的 `gate_tool_calls` /
//! `resolve_approvals` 系统里执行：deny 命中以合成失败结果推进机器，ask
//! 命中走审批口问询。这里只定义纯数据词汇。

use agnes_core::config::ConfigError;
use agnes_core::tool::ToolName;
use std::fmt;
use std::str::FromStr;

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
///
/// 配了 `ask` 规则就必须接通审批口，否则每次命中都只会 fail-closed 拒绝。
#[must_use = "insert the rules into the app as a resource"]
#[derive(Debug, Clone, Default)]
pub struct PermissionRules {
    pub deny: Vec<Rule>,
    pub ask: Vec<Rule>,
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
    fn rules_match_by_name_and_prefix() {
        let deny_prefix = Rule::Prefix("shell".into());
        assert!(deny_prefix.matches(&call("shell_find").name));
        assert!(!deny_prefix.matches(&call("read_file").name));

        let exact = Rule::Name(ToolName::from_str("run_shell").unwrap());
        assert!(exact.matches(&call("run_shell").name));
        assert!(!exact.matches(&call("run_shell_extra").name));
    }

    #[test]
    fn rules_parse_from_specs() {
        assert!(matches!(Rule::from_str("run_shell"), Ok(Rule::Name(_))));
        assert!(matches!(Rule::from_str("shell_*"), Ok(Rule::Prefix(_))));
        assert!(Rule::from_str("*").is_err());
        assert!(Rule::from_str("   ").is_err());
    }
}

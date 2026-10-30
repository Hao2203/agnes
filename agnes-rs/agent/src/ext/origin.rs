//! 来源标签：一条决议或消息由谁产生。

use cold_string::ArcColdString;
use std::fmt;

/// 一条决议/消息的来源标签：插件名，或系统的合成来源（如审批）。
///
/// 内核在注册时捕获插件名并盖章，插件无法伪造来源。名字通常很短
/// （不超过一个字长时内联，长名字引用计数）；克隆不分配，长名字只
/// 增加引用计数。内部表示不进入公共签名，对外只讲 `&str` / `String`。
#[derive(Clone, PartialEq, Eq)]
pub struct Origin(ArcColdString);

impl Origin {
    pub fn new(value: &str) -> Self {
        Self(ArcColdString::new(value))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl From<&str> for Origin {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Origin {
    fn from(value: String) -> Self {
        Self::new(&value)
    }
}

impl AsRef<str> for Origin {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Origin").field(&self.as_str()).finish()
    }
}

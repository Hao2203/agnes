//! 系统侧的拒绝词汇：一次拒绝记录谁拒的、为什么。

use crate::origin::Origin;

/// 一次拒绝：谁拒的、为什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    /// 拒绝者（注册时捕获的插件名）。
    pub by: Origin,
    /// 理由（随合成结果一起给到模型）。
    pub reason: String,
}

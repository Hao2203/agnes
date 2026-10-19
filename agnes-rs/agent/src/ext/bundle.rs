//! 装载单元：把一组注册与它们的装载期校验打包。
//!
//! dsh 的 profile/bundle 在 Rust 里的诚实形状 —— bundle 在 [`Host`] 装配
//! 时按注册顺序应用，自包含的配置错误（规则指向不存在的工具、配了 ask
//! 却没接审批口、工具重名）在装载期大声失败，而不是等到第一次调用。
//!
//! 长期注册留在宿主里；需要「随作用域撤销」的临时注册用
//! [`Extensions::scope`](crate::ext::Extensions::scope)。

use crate::host::{Host, LoadError};

/// 一组可复用的注册与校验。
pub trait Bundle {
    /// 装载单元的名字，用于错误信息。
    fn name(&self) -> &'static str;

    /// 把注册装进宿主；校验失败即中止装配。
    fn install(&self, host: &mut Host<'_>) -> Result<(), LoadError>;
}

/// 总在装配时可直接使用的空单元（组合的起点）。
#[derive(Debug, Default, Clone, Copy)]
pub struct Empty;

impl Bundle for Empty {
    fn name(&self) -> &'static str {
        "empty"
    }

    fn install(&self, _host: &mut Host<'_>) -> Result<(), LoadError> {
        Ok(())
    }
}

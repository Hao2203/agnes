//! 服务宿主的错误词汇。

use std::error::Error;

/// 插件 `apply` / `shutdown` 返回的错误：插件可以携带任意错误上下文。
pub type PluginError = Box<dyn Error + Send + Sync + 'static>;

/// 服务表上的显式失败。缺依赖不是错误 —— 它是 `Pending` 状态。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServiceError {
    #[error("service `{type_name}` is already provided")]
    ServiceTaken { type_name: &'static str },
    #[error("service `{type_name}` is not available")]
    MissingService { type_name: &'static str },
}

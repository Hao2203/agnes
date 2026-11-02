//! `agnes-service`：Cordis 风格的服务编排。
//!
//! - 服务键是 `TypeId`；查询/提供通过独立的 [`Services`] service，`Ctx` 只
//!   提供 `effect`。`dyn Trait` 也能直接作为服务类型。
//! - 事件是独立的 [`Events`] service；子插件挂载通过 [`Mounts`] service。
//! - 依赖由 `type Deps` 的类型自动推导：`Arc<T>` 是硬依赖，
//!   `Option<Arc<T>>` 是可选的，元组组合多个依赖；插件不写 `inject` 列表。
//! - 缺依赖时停在 `Pending` 并写 `log::warn!`，不报错；提供者卸载后依赖者
//!   回到 `Pending`，重新提供时复用同一个实例再次 `apply`。
//! - 生命周期是异步的：`mount` / `unmount` / `settle` / `shutdown`、
//!   `Plugin::apply` / `shutdown`、`waterfall` 都返回 future；crate 不绑定
//!   运行时，任意执行器都能驱动。
//! - 所有注册都是 effect：每个 fiber 一个 LIFO 容器；正常卸载时
//!   `guard.close().await`，取消或宿主 `Drop` 时只靠 guard 的 `Drop` 兜底。
//! - `emit` / `bail` 保持同步；`waterfall` 的监听器可以 `await next()`。
//!
//! 因此 `apply` 必须可重复调用，`shutdown` 必须幂等。未调用
//! `shutdown().await` 就丢弃宿主时，Drop 只回卷同步 effect，并记录一条警告。
//!
//! ```
//! use agnes_service::{Ctx, Plugin, PluginError, ServiceHost, async_trait};
//!
//! struct Hello;
//!
//! #[async_trait]
//! impl Plugin for Hello {
//!     type Deps = ();
//!
//!     async fn apply(&mut self, _ctx: &Ctx<'_>, _deps: ()) -> Result<(), PluginError> {
//!         log::info!("hello from my first plugin");
//!         Ok(())
//!     }
//! }
//!
//! futures_lite::future::block_on(async {
//!     let host = ServiceHost::new();
//!     host.mount(Hello).await;
//!     host.shutdown().await;
//! });
//! ```

mod ctx;
mod effect;
mod error;
mod event;
mod host;
mod mounts;
mod plugin;
mod service;

pub use async_trait::async_trait;
pub use ctx::Ctx;
pub use effect::EffectGuard;
pub use error::{PluginError, ServiceError};
pub use event::{
    BailKey, BailListener, EmitListener, EventKey, Events, Listener, Next, WaterfallKey,
    WaterfallListener,
};
pub use host::ServiceHost;
pub use mounts::Mounts;
pub use plugin::{Fiber, FiberInfo, FiberState, Plugin};
pub use service::{ServiceDep, ServiceInfo, ServiceParam, Services};

#[cfg(test)]
mod tests;

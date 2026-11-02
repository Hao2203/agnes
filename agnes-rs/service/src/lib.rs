//! `agnes-service`：Cordis 风格的服务编排。
//!
//! - 服务键是 `TypeId`：`ctx.provide(Arc<T>)` / `ctx.get::<T>()`，`dyn Trait`
//!   也能直接作为服务类型。
//! - 依赖由 `type Deps` 的类型自动推导：`Arc<T>` 是硬依赖，
//!   `Option<Arc<T>>` 是可选的，元组组合多个依赖；插件不写 `inject` 列表。
//! - 缺依赖时停在 `Pending` 并写 `log::warn!`，不报错；提供者卸载后依赖者
//!   回到 `Pending`，重新提供时复用同一个实例再次 `apply`。
//! - 生命周期是异步的：`mount` / `unmount` / `settle` / `shutdown`、
//!   `Plugin::apply` / `shutdown`、`waterfall` 都返回 future；crate 不绑定
//!   运行时，任意执行器都能驱动。
//! - 所有注册都是 effect：fiber 卸载时逆序回卷。effect disposer 是同步的；
//!   需要 `await` 的收尾放在 `Plugin::shutdown`。
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

mod effect;
mod error;
mod event;
mod host;
mod plugin;
mod service;

pub use async_trait::async_trait;
pub use effect::Disposer;
pub use error::{PluginError, ServiceError};
pub use event::{
    BailKey, BailListener, EmitListener, EventKey, Listener, Next, WaterfallKey, WaterfallListener,
    next,
};
pub use host::{Ctx, ServiceHost};
pub use plugin::{Fiber, FiberInfo, FiberState, Plugin};
pub use service::{ServiceDep, ServiceInfo, ServiceParam};

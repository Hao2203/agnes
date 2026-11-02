//! 插件、fiber 与类型擦除适配器。
//!
//! 插件的依赖由 `type Deps` 描述；宿主在激活前解析它，并把解析好的值传给
//! `apply`。挂载收实例，不使用闭包工厂；依赖恢复时复用同一个实例再次
//! `apply`，因此 `apply` 必须可重复调用，`shutdown` 必须幂等。

use std::sync::Arc;

use async_trait::async_trait;

use crate::ctx::Ctx;
use crate::error::PluginError;
use crate::service::{ServiceDep, ServiceParam};

/// 一个可挂载的插件。
#[async_trait]
pub trait Plugin: Send + 'static {
    /// 依赖集合的类型级描述。`Arc<T>` 是硬依赖，`Option<Arc<T>>` 是可选的。
    type Deps: ServiceParam;

    /// 诊断名；默认使用完整类型名。
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }

    /// 注册服务、监听器与 effect。
    ///
    /// `ctx` 只负责 `effect`；服务/事件/子挂载通过 `deps` 里的
    /// [`Services`](crate::Services)、[`Events`](crate::Events)、
    /// [`Mounts`](crate::Mounts) 完成。
    ///
    /// 依赖恢复后会再次调用；实现应把本次传入的 `deps` 作为唯一有效句柄，
    /// 不要在字段里缓存上一次的值。
    ///
    /// # Errors
    ///
    /// 返回错误会让 fiber 进入 `Failed`，调用一次 [`Plugin::shutdown`]，
    /// 并回卷本次已登记的 effect。
    async fn apply(
        &mut self,
        ctx: &Ctx<'_>,
        deps: <<Self as Plugin>::Deps as ServiceParam>::Output,
    ) -> Result<(), PluginError>;

    /// 异步清理钩子。
    ///
    /// 可能在 `apply` 未运行、或已经运行过多次时被调用；实现必须幂等，只释放
    /// 自己的资源，不依赖上一次 `apply` 的状态。
    ///
    /// # Errors
    ///
    /// 清理失败只记录日志，不影响 effect 的回卷。
    async fn shutdown(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}

/// 一个已挂载插件实例的句柄。带上宿主戳，不能跨宿主使用。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Fiber {
    pub(crate) host: u64,
    pub(crate) id: usize,
}

impl Fiber {
    pub(crate) fn new(host: u64, id: usize) -> Self {
        Self { host, id }
    }

    pub(crate) fn id(self) -> usize {
        self.id
    }

    pub(crate) fn host(self) -> u64 {
        self.host
    }

    pub(crate) fn is_root(self) -> bool {
        self.id == 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum FiberState {
    /// 已挂载，但依赖（或父插件）尚未就绪；这是合法状态。
    Pending,
    /// `apply` 正在运行；只在重入观察时短暂可见。
    Loading,
    /// `apply` 已成功完成。
    Active,
    /// 已卸载，effect 已回卷。
    Disposed,
    /// `apply` 返回了错误，effect 已回卷；需要显式卸载后重挂。
    Failed,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct FiberInfo {
    pub fiber: Fiber,
    pub name: String,
    pub state: FiberState,
    /// 仅 `Pending` 时有值：还缺哪些服务。
    pub missing: Vec<ServiceDep>,
    /// `apply` 的错误；`Failed` 以及由失败转入 `Disposed` 的 fiber 上保留。
    pub error: Option<Arc<PluginError>>,
}

/// 适配器内部的失败：区分“依赖在 check 与 resolve 之间消失”和插件错误。
pub(crate) enum ApplyError {
    MissingDeps,
    Plugin(PluginError),
}

/// 类型擦除后的插件：宿主只存这个 trait object。
#[async_trait]
pub(crate) trait ErasedPlugin: Send {
    async fn apply(&mut self, ctx: &Ctx<'_>) -> Result<(), ApplyError>;
    async fn shutdown(&mut self) -> Result<(), PluginError>;
}

pub(crate) struct Adapter<P: Plugin>(P);

impl<P: Plugin> Adapter<P> {
    pub(crate) fn new(plugin: P) -> Self {
        Self(plugin)
    }
}

#[async_trait]
impl<P: Plugin> ErasedPlugin for Adapter<P> {
    async fn apply(&mut self, ctx: &Ctx<'_>) -> Result<(), ApplyError> {
        let deps = <P::Deps as ServiceParam>::resolve(ctx).ok_or(ApplyError::MissingDeps)?;
        self.0.apply(ctx, deps).await.map_err(ApplyError::Plugin)
    }

    async fn shutdown(&mut self) -> Result<(), PluginError> {
        self.0.shutdown().await
    }
}

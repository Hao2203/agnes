//! 插件作用域内的上下文。

use std::any::TypeId;
use std::sync::Arc;

use crate::effect::{Disposer, Effect};
use crate::error::ServiceError;
use crate::event::{self, BailKey, EventKey, Listener, Next, WaterfallKey};
use crate::host::ServiceHost;
use crate::plugin::{Fiber, Plugin};

/// 插件作用域内的上下文。借用宿主，不能跨 `apply` 保存。
#[derive(Clone, Copy)]
pub struct Ctx<'h> {
    host: &'h ServiceHost,
    fiber: Fiber,
}

impl<'h> Ctx<'h> {
    pub(crate) fn new(host: &'h ServiceHost, fiber: Fiber) -> Self {
        Self { host, fiber }
    }

    pub fn fiber(&self) -> Fiber {
        self.fiber
    }

    pub fn host(&self) -> &'h ServiceHost {
        self.host
    }

    /// 以当前 `TypeId` 注册服务。
    ///
    /// # Errors
    ///
    /// 同一类型已经有提供者时返回 [`ServiceError::ServiceTaken`]。
    pub async fn provide<T>(&self, service: Arc<T>) -> Result<(), ServiceError>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let id = TypeId::of::<T>();
        let owner = self.fiber;
        {
            let mut inner = self.host.lock();
            inner.services.provide::<T>(owner, service)?;
            inner
                .effects
                .entry(owner)
                .or_default()
                .push(Effect::RemoveService {
                    id,
                    provider: owner,
                });
        }
        self.host.settle_if_idle().await;
        Ok(())
    }

    /// 查找服务；不存在时返回 `None`。可选依赖用这个。
    pub fn get<T>(&self) -> Option<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.host.lock().services.get::<T>()
    }

    /// 查找服务；不存在时报错。
    ///
    /// # Errors
    ///
    /// 服务不存在时返回 [`ServiceError::MissingService`]。
    pub fn require<T>(&self) -> Result<Arc<T>, ServiceError>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.get::<T>().ok_or(ServiceError::MissingService {
            type_name: std::any::type_name::<T>(),
        })
    }

    /// 登记一个自定义资源；返回 setup 产出的值，卸载时逆序执行 disposer。
    pub fn effect<T>(&self, setup: impl FnOnce() -> (T, Disposer)) -> T {
        let (value, disposer) = setup();
        {
            let mut inner = self.host.lock();
            inner
                .effects
                .entry(self.fiber)
                .or_default()
                .push(Effect::Custom(disposer));
        }
        value
    }

    /// 在当前 fiber 下挂载子插件；父 fiber 卸载时子插件一并卸载。
    pub async fn mount<P: Plugin>(&self, plugin: P) -> Fiber {
        let fiber = self.host.mount_child(self.fiber, plugin);
        self.host.settle_if_idle().await;
        fiber
    }

    /// 同步广播；返回值被忽略。
    pub fn emit<E: 'static>(&self, key: EventKey<E>, event: &E) {
        event::emit(self.host, key, event);
    }

    /// 注册广播监听器，追加在末尾。
    pub fn on<E: 'static>(
        &self,
        key: EventKey<E>,
        listener: impl for<'a, 'b, 'c> Fn(&'a Ctx<'c>, &'b E) + Send + Sync + 'static,
    ) -> Listener {
        event::on(self.host, self.fiber, key, listener, false)
    }

    /// 注册广播监听器，插到最前面。
    pub fn prepend<E: 'static>(
        &self,
        key: EventKey<E>,
        listener: impl for<'a, 'b, 'c> Fn(&'a Ctx<'c>, &'b E) + Send + Sync + 'static,
    ) -> Listener {
        event::on(self.host, self.fiber, key, listener, true)
    }

    /// 广播决策：第一个返回 `Some` 的监听器胜出。
    pub fn bail<E: 'static, R: 'static>(&self, key: BailKey<E, R>, event: &E) -> Option<R> {
        event::bail(self.host, key, event)
    }

    pub fn on_bail<E: 'static, R: 'static>(
        &self,
        key: BailKey<E, R>,
        listener: impl for<'a, 'b, 'c> Fn(&'a Ctx<'c>, &'b E) -> Option<R> + Send + Sync + 'static,
    ) -> Listener {
        event::on_bail(self.host, self.fiber, key, listener, false)
    }

    pub fn prepend_bail<E: 'static, R: 'static>(
        &self,
        key: BailKey<E, R>,
        listener: impl for<'a, 'b, 'c> Fn(&'a Ctx<'c>, &'b E) -> Option<R> + Send + Sync + 'static,
    ) -> Listener {
        event::on_bail(self.host, self.fiber, key, listener, true)
    }

    /// 环绕中间件；监听器按注册顺序由外向内包装。
    pub async fn waterfall<E: Send + Sync + 'static, R: 'static>(
        &self,
        key: WaterfallKey<E, R>,
        event: &E,
        default: impl FnOnce() -> R + Send,
    ) -> R {
        event::waterfall(self.host, key, event, default).await
    }

    pub fn on_waterfall<E: Send + Sync + 'static, R: 'static>(
        &self,
        key: WaterfallKey<E, R>,
        listener: impl for<'a, 'c> Fn(&'a Ctx<'c>, &'a E, Next<'a, R>) -> Next<'a, R>
        + Send
        + Sync
        + 'static,
    ) -> Listener {
        event::on_waterfall(self.host, self.fiber, key, listener, false)
    }

    pub fn prepend_waterfall<E: Send + Sync + 'static, R: 'static>(
        &self,
        key: WaterfallKey<E, R>,
        listener: impl for<'a, 'c> Fn(&'a Ctx<'c>, &'a E, Next<'a, R>) -> Next<'a, R>
        + Send
        + Sync
        + 'static,
    ) -> Listener {
        event::on_waterfall(self.host, self.fiber, key, listener, true)
    }

    /// 提前撤销一个监听器；已撤销或不属于本宿主时返回 `false`。
    pub fn off(&self, listener: Listener) -> bool {
        let mut inner = self.host.lock();
        event::remove_listener(&mut inner, listener)
    }
}

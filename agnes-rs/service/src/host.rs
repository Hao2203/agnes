//! 服务宿主：服务表、fiber 生命周期、effect 回卷与诊断。
//!
//! 所有用户代码（插件的 `apply` / `shutdown`、事件监听器、自定义 disposer）
//! 都在锁外执行；锁只保护注册表本身，绝不跨 `.await` 持有。

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use log::{error, info, warn};

use crate::effect::{Disposer, Effect};
use crate::error::ServiceError;
use crate::event::{
    self, BailKey, EventKey, Listener, ListenerSet, ListenerSetId, Next, WaterfallKey,
};
use crate::plugin::{Adapter, ErasedPlugin, Fiber, FiberInfo, FiberState, Plugin};
use crate::service::{ServiceDep, ServiceInfo, ServiceParam, ServiceTable};

static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);
const TRANSITION_BUDGET: usize = 1024;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 服务与插件的容器。克隆句柄共享同一个宿主。
#[derive(Clone)]
pub struct ServiceHost {
    stamp: u64,
    shared: Arc<Shared>,
}

struct Shared {
    inner: Mutex<Inner>,
}

pub(crate) struct Inner {
    pub(crate) stamp: u64,
    services: ServiceTable,
    fibers: Vec<FiberSlot>,
    pub(crate) listeners: HashMap<ListenerSetId, Box<dyn Any + Send + Sync>>,
    pub(crate) effects: HashMap<Fiber, Vec<Effect>>,
    next_fiber: usize,
    pub(crate) next_listener: usize,
    settling: bool,
    shutdown: bool,
}

struct FiberSlot {
    name: String,
    deps: Vec<ServiceDep>,
    state: FiberState,
    parent: Option<Fiber>,
    plugin: Option<Box<dyn ErasedPlugin>>,
    error: Option<String>,
    dispose_requested: bool,
}

enum StepOutcome {
    Active(String),
    Failed { name: String, message: String },
    Retired(String),
}

impl Default for ServiceHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceHost {
    pub fn new() -> Self {
        let stamp = NEXT_STAMP.fetch_add(1, Ordering::Relaxed);
        Self {
            stamp,
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner::new(stamp)),
            }),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 根上下文；在这里注册的服务对所有插件可见。
    pub fn ctx(&self) -> Ctx<'_> {
        Ctx {
            host: self,
            fiber: Fiber::new(self.stamp, 0),
        }
    }

    /// 挂载一个插件实例。缺依赖时返回的 fiber 处于 `Pending`，不会报错。
    pub async fn mount<P: Plugin>(&self, plugin: P) -> Fiber {
        let fiber = self.mount_child(Fiber::new(self.stamp, 0), plugin);
        self.settle_if_idle().await;
        fiber
    }

    fn mount_child<P: Plugin>(&self, parent: Fiber, plugin: P) -> Fiber {
        let name = plugin.name().to_owned();
        let deps = <P::Deps as ServiceParam>::deps();
        let erased: Box<dyn ErasedPlugin> = Box::new(Adapter::new(plugin));

        let mut inner = self.lock();
        let id = inner.next_fiber;
        inner.next_fiber += 1;
        let fiber = Fiber::new(inner.stamp, id);

        if inner.shutdown {
            inner.fibers.push(FiberSlot {
                name: name.clone(),
                deps,
                state: FiberState::Disposed,
                parent: Some(parent),
                plugin: None,
                error: None,
                dispose_requested: false,
            });
            drop(inner);
            warn!("cannot mount `{name}`: the service host is shut down");
            return fiber;
        }

        let missing = inner.missing_deps(&deps);
        if parent.id() != 0 {
            inner
                .effects
                .entry(parent)
                .or_default()
                .push(Effect::DisposeChild(fiber));
        }
        inner.fibers.push(FiberSlot {
            name: name.clone(),
            deps,
            state: FiberState::Pending,
            parent: Some(parent),
            plugin: Some(erased),
            error: None,
            dispose_requested: false,
        });
        drop(inner);

        if !missing.is_empty() {
            let names = missing
                .iter()
                .map(|dep| dep.name())
                .collect::<Vec<_>>()
                .join(", ");
            warn!("plugin `{name}` is pending: missing {names}");
        }
        fiber
    }

    /// 卸载一个 fiber，并级联卸载依赖它服务的插件。
    ///
    /// 幂等：已经卸载或不属于本宿主的 fiber 返回 `false`。
    pub async fn unmount(&self, fiber: Fiber) -> bool {
        let changed = self.unmount_inner(fiber).await;
        if changed {
            self.settle_if_idle().await;
        }
        changed
    }

    fn unmount_inner(&self, target: Fiber) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            if target.host() != self.stamp {
                return false;
            }
            let index = target.id();
            let loading = {
                let inner = self.lock();
                let Some(slot) = inner.fibers.get(index) else {
                    return false;
                };
                if index == 0 || slot.state == FiberState::Disposed {
                    return false;
                }
                slot.state == FiberState::Loading
            };
            if loading {
                self.lock().fibers[index].dispose_requested = true;
                return true;
            }

            for (fiber, dispose) in self.compute_victims(target) {
                let (mut plugin, effects, name) = {
                    let mut inner = self.lock();
                    let slot = &mut inner.fibers[fiber.id()];
                    if slot.state == FiberState::Disposed {
                        continue;
                    }
                    slot.state = if dispose {
                        FiberState::Disposed
                    } else {
                        FiberState::Pending
                    };
                    let name = slot.name.clone();
                    let plugin = slot.plugin.take();
                    let effects = inner.effects.remove(&fiber).unwrap_or_default();
                    (plugin, effects, name)
                };

                if !dispose {
                    warn!("plugin `{name}` is pending: a required service was removed");
                }

                if let Some(plugin) = plugin.as_mut()
                    && let Err(error) = plugin.shutdown().await
                {
                    warn!("plugin `{name}` shutdown failed: {error}");
                }

                if dispose {
                    drop(plugin);
                } else {
                    let mut inner = self.lock();
                    if inner.fibers[fiber.id()].state == FiberState::Pending {
                        inner.fibers[fiber.id()].plugin = plugin;
                    }
                }

                self.run_effects(effects).await;
            }
            true
        })
    }

    fn compute_victims(&self, target: Fiber) -> Vec<(Fiber, bool)> {
        let inner = self.lock();
        let mut dispose = Vec::new();
        let mut stack = vec![target];
        while let Some(fiber) = stack.pop() {
            if dispose.contains(&fiber) {
                continue;
            }
            dispose.push(fiber);
            for (index, slot) in inner.fibers.iter().enumerate() {
                if slot.parent == Some(fiber) {
                    stack.push(Fiber::new(inner.stamp, index));
                }
            }
        }

        let mut victims = dispose.clone();
        loop {
            let provided: Vec<TypeId> = inner
                .services
                .entries()
                .filter_map(|(id, entry)| victims.contains(&entry.provider).then_some(*id))
                .collect();
            let mut added = false;
            for (index, slot) in inner.fibers.iter().enumerate() {
                if index == 0 {
                    continue;
                }
                let fiber = Fiber::new(inner.stamp, index);
                if victims.contains(&fiber) || slot.state != FiberState::Active {
                    continue;
                }
                if slot.deps.iter().any(|dep| provided.contains(&dep.id())) {
                    victims.push(fiber);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }

        let mut remaining: Vec<(Fiber, bool)> = victims
            .iter()
            .map(|fiber| (*fiber, dispose.contains(fiber)))
            .collect();
        let mut ordered = Vec::with_capacity(remaining.len());
        while !remaining.is_empty() {
            let chosen = remaining
                .iter()
                .position(|(candidate, _)| {
                    !remaining.iter().any(|(other, _)| {
                        other != candidate && inner.depends_on(*other, *candidate)
                    })
                })
                .unwrap_or(0);
            ordered.push(remaining.remove(chosen));
        }
        ordered
    }

    fn run_effects(&self, mut effects: Vec<Effect>) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            while let Some(effect) = effects.pop() {
                match effect {
                    Effect::RemoveService { id, provider } => {
                        self.lock().services.remove(id, provider);
                    }
                    Effect::RemoveListener(listener) => {
                        let mut inner = self.lock();
                        event::remove_listener(&mut inner, listener);
                    }
                    Effect::DisposeChild(child) => {
                        self.unmount_inner(child).await;
                    }
                    Effect::Custom(disposer) => disposer(),
                }
            }
        })
    }

    /// 把所有可激活的 `Pending` fiber 推进到 `Active`。
    pub async fn settle(&self) {
        self.settle_inner().await;
    }

    async fn settle_if_idle(&self) {
        if self.lock().settling {
            return;
        }
        self.settle_inner().await;
    }

    async fn settle_inner(&self) {
        {
            let mut inner = self.lock();
            if inner.settling {
                return;
            }
            inner.settling = true;
        }
        let _guard = SettlingGuard { host: self };
        let mut transitions = 0usize;

        loop {
            if transitions >= TRANSITION_BUDGET {
                error!("settle exceeded the transition budget");
                break;
            }
            let fiber = {
                let mut inner = self.lock();
                inner.next_activatable()
            };
            let Some(fiber) = fiber else {
                break;
            };
            transitions += 1;

            let mut instance = {
                let mut inner = self.lock();
                inner.fibers[fiber.id()].plugin.take()
            };
            let Some(current) = instance.as_mut() else {
                error!("plugin `{}` has no instance before apply", fiber.id());
                break;
            };

            let ctx = Ctx::new(self, fiber);
            let result = current.apply(&ctx).await;

            let (outcome, effects, needs_shutdown) = {
                let mut inner = self.lock();
                let slot = &mut inner.fibers[fiber.id()];
                let name = slot.name.clone();
                match result {
                    Ok(()) if slot.dispose_requested => {
                        slot.dispose_requested = false;
                        slot.state = FiberState::Disposed;
                        let effects = inner.effects.remove(&fiber).unwrap_or_default();
                        (StepOutcome::Retired(name), effects, true)
                    }
                    Ok(()) => {
                        slot.plugin = instance.take();
                        slot.state = FiberState::Active;
                        (StepOutcome::Active(name), Vec::new(), false)
                    }
                    Err(error) => {
                        let message = error.to_string();
                        slot.error = Some(message.clone());
                        slot.state = FiberState::Failed;
                        let effects = inner.effects.remove(&fiber).unwrap_or_default();
                        (StepOutcome::Failed { name, message }, effects, true)
                    }
                }
            };

            match &outcome {
                StepOutcome::Active(name) => info!("plugin `{name}` is active"),
                StepOutcome::Failed { name, message } => {
                    error!("plugin `{name}` failed: {message}");
                }
                StepOutcome::Retired(name) => info!("plugin `{name}` disposed"),
            }

            if needs_shutdown
                && let Some(mut plugin) = instance
                && let Err(error) = plugin.shutdown().await
            {
                warn!("plugin shutdown after failure failed: {error}");
            }

            self.run_effects(effects).await;
        }
    }

    pub fn fiber(&self, fiber: Fiber) -> Option<FiberInfo> {
        if fiber.host() != self.stamp {
            return None;
        }
        let inner = self.lock();
        if fiber.id() >= inner.fibers.len() {
            return None;
        }
        Some(inner.fiber_info(fiber))
    }

    /// 所有非根 fiber 的诊断快照，按挂载顺序。
    pub fn fibers(&self) -> Vec<FiberInfo> {
        let inner = self.lock();
        (1..inner.fibers.len())
            .map(|index| inner.fiber_info(Fiber::new(inner.stamp, index)))
            .collect()
    }

    pub fn services(&self) -> Vec<ServiceInfo> {
        let inner = self.lock();
        let mut services: Vec<ServiceInfo> = inner
            .services
            .entries()
            .map(|(_, entry)| ServiceInfo {
                type_name: entry.type_name,
                provider: entry.provider,
                provider_name: inner.fibers[entry.provider.id()].name.clone(),
            })
            .collect();
        services.sort_by_key(|info| info.type_name);
        services
    }

    /// 卸载所有插件并回卷根 effect；幂等。
    pub async fn shutdown(&self) {
        {
            let mut inner = self.lock();
            if inner.shutdown {
                return;
            }
            inner.shutdown = true;
        }

        let fibers: Vec<Fiber> = {
            let inner = self.lock();
            (1..inner.fibers.len())
                .rev()
                .map(|index| Fiber::new(inner.stamp, index))
                .collect()
        };
        for fiber in fibers {
            self.unmount_inner(fiber).await;
        }

        let root = Fiber::new(self.stamp, 0);
        let effects = self.lock().effects.remove(&root).unwrap_or_default();
        self.run_effects(effects).await;
        info!("service host shut down");
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let inner = self
            .inner
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !inner.shutdown {
            warn!("service host dropped without shutdown; async plugin cleanup was skipped");
        }
        let stamp = inner.stamp;
        let mut indexes: Vec<usize> = (0..inner.fibers.len()).collect();
        indexes.sort_unstable_by(|a, b| b.cmp(a));
        for index in indexes {
            let fiber = Fiber::new(stamp, index);
            let effects = inner.effects.remove(&fiber).unwrap_or_default();
            drop_effects(inner, effects);
            inner.fibers[index].plugin = None;
        }
    }
}

fn drop_effects(inner: &mut Inner, mut effects: Vec<Effect>) {
    while let Some(effect) = effects.pop() {
        match effect {
            Effect::RemoveService { id, provider } => {
                inner.services.remove(id, provider);
            }
            Effect::RemoveListener(listener) => {
                event::remove_listener(inner, listener);
            }
            Effect::DisposeChild(_) => {}
            Effect::Custom(disposer) => disposer(),
        }
    }
}

struct SettlingGuard<'a> {
    host: &'a ServiceHost,
}

impl Drop for SettlingGuard<'_> {
    fn drop(&mut self) {
        self.host.lock().settling = false;
    }
}

impl Inner {
    fn new(stamp: u64) -> Self {
        Self {
            stamp,
            services: ServiceTable::default(),
            fibers: vec![FiberSlot {
                name: "root".into(),
                deps: Vec::new(),
                state: FiberState::Active,
                parent: None,
                plugin: None,
                error: None,
                dispose_requested: false,
            }],
            listeners: HashMap::new(),
            effects: HashMap::new(),
            next_fiber: 1,
            next_listener: 0,
            settling: false,
            shutdown: false,
        }
    }

    fn available(&self, dep: ServiceDep) -> bool {
        self.services
            .provider(dep.id())
            .is_some_and(|provider| self.fibers[provider.id()].state == FiberState::Active)
    }

    fn missing_deps(&self, deps: &[ServiceDep]) -> Vec<ServiceDep> {
        deps.iter()
            .copied()
            .filter(|dep| !self.available(*dep))
            .collect()
    }

    fn missing(&self, fiber: Fiber) -> Vec<ServiceDep> {
        self.missing_deps(&self.fibers[fiber.id()].deps)
    }

    fn fiber_info(&self, fiber: Fiber) -> FiberInfo {
        let slot = &self.fibers[fiber.id()];
        FiberInfo {
            fiber,
            name: slot.name.clone(),
            state: slot.state,
            missing: if slot.state == FiberState::Pending {
                self.missing(fiber)
            } else {
                Vec::new()
            },
            error: slot.error.clone(),
        }
    }

    fn next_activatable(&mut self) -> Option<Fiber> {
        let mut selected = None;
        for (index, slot) in self.fibers.iter().enumerate().skip(1) {
            if slot.state != FiberState::Pending {
                continue;
            }
            if let Some(parent) = slot.parent
                && self.fibers[parent.id()].state != FiberState::Active
            {
                continue;
            }
            if slot.plugin.is_none() {
                continue;
            }
            if slot.deps.iter().any(|dep| !self.available(*dep)) {
                continue;
            }
            selected = Some(index);
            break;
        }

        let index = selected?;
        self.fibers[index].state = FiberState::Loading;
        Some(Fiber::new(self.stamp, index))
    }

    pub(crate) fn listener_set_mut<F: ?Sized + Send + Sync + 'static>(
        &mut self,
        id: ListenerSetId,
    ) -> &mut ListenerSet<F> {
        let boxed = self
            .listeners
            .entry(id)
            .or_insert_with(|| Box::new(ListenerSet::<F>::default()));
        let any: &mut dyn Any = &mut **boxed;
        any.downcast_mut::<ListenerSet<F>>()
            .expect("listener set type mismatch")
    }

    pub(crate) fn listener_set<F: ?Sized + Send + Sync + 'static>(
        &self,
        id: ListenerSetId,
    ) -> Option<&ListenerSet<F>> {
        let any: &dyn Any = &**self.listeners.get(&id)?;
        any.downcast_ref::<ListenerSet<F>>()
    }

    fn depends_on(&self, dependent: Fiber, provider: Fiber) -> bool {
        if self.fibers[dependent.id()].parent == Some(provider) {
            return true;
        }
        self.fibers[dependent.id()]
            .deps
            .iter()
            .any(|dep| self.services.provider(dep.id()) == Some(provider))
    }
}

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

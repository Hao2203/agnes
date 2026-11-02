//! 服务宿主：服务表、fiber 生命周期、effect 回卷与诊断。
//!
//! 所有用户代码（插件的 `apply` / `shutdown`、effect guard 的 `close` /
//! `Drop`）都在锁外执行；锁只保护注册表本身，绝不跨 `.await` 持有。

use std::any::TypeId;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use log::{error, info, warn};

use crate::ctx::Ctx;
use crate::effect::Effects;
use crate::error::PluginError;
use crate::event::Events;
use crate::mounts::Mounts;
use crate::plugin::{Adapter, ApplyError, ErasedPlugin, Fiber, FiberInfo, FiberState, Plugin};
use crate::service::{ServiceDep, ServiceInfo, ServiceParam, ServiceTable, Services};

static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);
const TRANSITION_BUDGET: usize = 1024;
const ROOT_INDEX: usize = 0;
const FIRST_CHILD: usize = ROOT_INDEX + 1;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 服务与插件的容器。克隆句柄共享同一个宿主。
#[derive(Clone)]
pub struct ServiceHost {
    stamp: u64,
    shared: Arc<Shared>,
}

pub(crate) struct Shared {
    inner: Mutex<Inner>,
}

pub(crate) struct Inner {
    pub(crate) stamp: u64,
    pub(crate) services: ServiceTable,
    fibers: Vec<FiberSlot>,
    pub(crate) effects: HashMap<Fiber, Effects>,
    next_fiber: usize,
    settling: bool,
    shutdown: bool,
}

struct FiberSlot {
    name: String,
    deps: Vec<ServiceDep>,
    state: FiberState,
    parent: Option<Fiber>,
    plugin: Option<Box<dyn ErasedPlugin>>,
    error: Option<Arc<PluginError>>,
    dispose_requested: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Dispose,
    Pend,
}

enum StepOutcome {
    Active(String),
    Pending(String),
    Failed {
        name: String,
        error: Arc<PluginError>,
    },
    Retired,
}

fn install_root<T: ?Sized + Send + Sync + 'static>(inner: &mut Inner, root: Fiber, value: Arc<T>) {
    if let Err(error) = inner.services.provide::<T>(root, value) {
        unreachable!("fresh service table rejected a root service: {error}");
    }
}

impl Default for ServiceHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceHost {
    pub fn new() -> Self {
        let stamp = NEXT_STAMP.fetch_add(1, Ordering::Relaxed);
        let host = Self {
            stamp,
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner::new(stamp)),
            }),
        };
        let root = Fiber::new(stamp, ROOT_INDEX);
        {
            let mut inner = host.lock();
            install_root(&mut inner, root, Arc::new(Services));
            install_root(&mut inner, root, Arc::new(Mounts));
            install_root(&mut inner, root, Arc::new(Events::default()));
        }
        host
    }

    pub(crate) fn downgrade(&self) -> Weak<Shared> {
        Arc::downgrade(&self.shared)
    }

    pub(crate) fn from_shared(shared: Arc<Shared>) -> Self {
        let stamp = shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stamp;
        Self { stamp, shared }
    }

    #[cfg(test)]
    pub(crate) fn resolve<T>(&self) -> Option<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.lock().services.get::<T>()
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 根上下文；在这里注册的服务对所有插件可见。
    pub fn ctx(&self) -> Ctx<'_> {
        Ctx::new(self, Fiber::new(self.stamp, ROOT_INDEX))
    }

    /// 挂载一个插件实例。缺依赖时返回的 fiber 处于 `Pending`，不会报错。
    pub async fn mount<P: Plugin>(&self, plugin: P) -> Fiber {
        let fiber = self.mount_child(Fiber::new(self.stamp, ROOT_INDEX), plugin);
        self.settle_if_idle().await;
        fiber
    }

    pub(crate) fn mount_child<P: Plugin>(&self, parent: Fiber, plugin: P) -> Fiber {
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

    pub(crate) fn unmount_inner(&self, target: Fiber) -> BoxFuture<'_, bool> {
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
                if target.is_root() || slot.state == FiberState::Disposed {
                    return false;
                }
                slot.state == FiberState::Loading
            };
            if loading {
                self.lock().fibers[index].dispose_requested = true;
                return true;
            }

            for (fiber, disposition) in self.compute_victims(target) {
                let (mut plugin, mut effects, name) = {
                    let mut inner = self.lock();
                    let slot = &mut inner.fibers[fiber.id()];
                    if slot.state == FiberState::Disposed {
                        continue;
                    }
                    slot.state = match disposition {
                        Disposition::Dispose => FiberState::Disposed,
                        Disposition::Pend => FiberState::Pending,
                    };
                    let name = slot.name.clone();
                    let plugin = slot.plugin.take();
                    let effects = inner.effects.remove(&fiber).unwrap_or_default();
                    (plugin, effects, name)
                };

                if matches!(disposition, Disposition::Pend) {
                    warn!("plugin `{name}` is pending: a required service was removed");
                }

                if let Some(plugin) = plugin.as_mut()
                    && let Err(error) = plugin.shutdown().await
                {
                    warn!("plugin `{name}` shutdown failed: {error}");
                }

                if matches!(disposition, Disposition::Dispose) {
                    drop(plugin);
                } else {
                    let mut inner = self.lock();
                    if inner.fibers[fiber.id()].state == FiberState::Pending {
                        inner.fibers[fiber.id()].plugin = plugin;
                    }
                }

                effects.close().await;
            }
            true
        })
    }

    fn compute_victims(&self, target: Fiber) -> Vec<(Fiber, Disposition)> {
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
                let fiber = Fiber::new(inner.stamp, index);
                if fiber.is_root() {
                    continue;
                }
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

        let mut remaining: Vec<(Fiber, Disposition)> = victims
            .iter()
            .map(|fiber| {
                let disposition = if dispose.contains(fiber) {
                    Disposition::Dispose
                } else {
                    Disposition::Pend
                };
                (*fiber, disposition)
            })
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

    /// 同步销毁一个 fiber；异步 `shutdown` 无法在取消路径里 await，会被跳过。
    pub(crate) fn dispose_fiber_sync(&self, fiber: Fiber) {
        let (plugin, effects, name) = {
            let mut inner = self.lock();
            let Some(slot) = inner.fibers.get_mut(fiber.id()) else {
                return;
            };
            if slot.state == FiberState::Disposed {
                return;
            }
            slot.state = FiberState::Disposed;
            let name = slot.name.clone();
            let plugin = slot.plugin.take();
            let effects = inner.effects.remove(&fiber);
            (plugin, effects, name)
        };
        if plugin.is_some() {
            warn!("plugin `{name}` was disposed synchronously; async shutdown was skipped");
        }
        drop(plugin);
        drop(effects);
    }

    /// 把所有可激活的 `Pending` fiber 推进到 `Active`。
    pub async fn settle(&self) {
        self.settle_inner().await;
    }

    pub(crate) async fn settle_if_idle(&self) {
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

            let instance = {
                let mut inner = self.lock();
                inner.fibers[fiber.id()].plugin.take()
            };
            let Some(instance) = instance else {
                let name = {
                    let inner = self.lock();
                    inner.fibers[fiber.id()].name.clone()
                };
                error!("plugin `{name}` has no instance before apply");
                self.lock().fibers[fiber.id()].state = FiberState::Failed;
                continue;
            };

            let ctx = Ctx::new(self, fiber);
            let mut guard = ApplyGuard::new(self, fiber, instance);
            let result = guard.plugin_mut().apply(&ctx).await;
            guard.commit();
            let mut instance = guard.take();

            let (outcome, effects, needs_shutdown) = {
                let mut inner = self.lock();
                let slot = &mut inner.fibers[fiber.id()];
                let name = slot.name.clone();
                match result {
                    Ok(()) if slot.dispose_requested => {
                        slot.dispose_requested = false;
                        slot.state = FiberState::Disposed;
                        let effects = inner.effects.remove(&fiber);
                        (StepOutcome::Retired, effects, true)
                    }
                    Ok(()) => {
                        slot.plugin = instance.take();
                        slot.state = FiberState::Active;
                        (StepOutcome::Active(name), None, false)
                    }
                    Err(ApplyError::MissingDeps) => {
                        if slot.dispose_requested {
                            slot.dispose_requested = false;
                            slot.state = FiberState::Disposed;
                            let effects = inner.effects.remove(&fiber);
                            (StepOutcome::Retired, effects, false)
                        } else {
                            slot.plugin = instance.take();
                            slot.state = FiberState::Pending;
                            let effects = inner.effects.remove(&fiber);
                            (StepOutcome::Pending(name), effects, false)
                        }
                    }
                    Err(ApplyError::Plugin(error)) => {
                        let error = Arc::new(error);
                        slot.error = Some(Arc::clone(&error));
                        slot.state = FiberState::Failed;
                        let effects = inner.effects.remove(&fiber);
                        (StepOutcome::Failed { name, error }, effects, true)
                    }
                }
            };

            match &outcome {
                StepOutcome::Active(name) => info!("plugin `{name}` is active"),
                StepOutcome::Pending(name) => {
                    warn!("plugin `{name}` is pending: dependencies vanished before apply");
                }
                StepOutcome::Failed { name, error } => {
                    error!("plugin `{name}` failed: {error}");
                }
                StepOutcome::Retired => {}
            }

            if needs_shutdown
                && let Some(mut plugin) = instance
                && let Err(error) = plugin.shutdown().await
            {
                warn!("plugin shutdown after failure failed: {error}");
            }

            if let Some(mut effects) = effects {
                effects.close().await;
            }
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
        (FIRST_CHILD..inner.fibers.len())
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
            (FIRST_CHILD..inner.fibers.len())
                .rev()
                .map(|index| Fiber::new(inner.stamp, index))
                .collect()
        };
        for fiber in fibers {
            self.unmount_inner(fiber).await;
        }

        let root = Fiber::new(self.stamp, ROOT_INDEX);
        let effects = self.lock().effects.remove(&root);
        if let Some(mut effects) = effects {
            effects.close().await;
        }
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
            let effects = inner.effects.remove(&fiber);
            drop(effects);
            inner.fibers[index].plugin = None;
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

/// 保证 `apply` future 被取消时，插件实例和 fiber 状态不会卡在 `Loading`。
struct ApplyGuard<'a> {
    host: &'a ServiceHost,
    fiber: Fiber,
    plugin: Option<Box<dyn ErasedPlugin>>,
    committed: bool,
}

impl<'a> ApplyGuard<'a> {
    fn new(host: &'a ServiceHost, fiber: Fiber, plugin: Box<dyn ErasedPlugin>) -> Self {
        Self {
            host,
            fiber,
            plugin: Some(plugin),
            committed: false,
        }
    }

    fn plugin_mut(&mut self) -> &mut Box<dyn ErasedPlugin> {
        self.plugin
            .as_mut()
            .expect("apply guard owns its plugin until commit")
    }

    fn commit(&mut self) {
        self.committed = true;
    }

    fn take(&mut self) -> Option<Box<dyn ErasedPlugin>> {
        self.plugin.take()
    }
}

impl Drop for ApplyGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let plugin = self.plugin.take();
        let (dispose, name) = {
            let mut inner = self.host.lock();
            match inner.fibers.get_mut(self.fiber.id()) {
                Some(slot) if slot.state == FiberState::Loading => {
                    slot.plugin = plugin;
                    slot.state = FiberState::Pending;
                    (slot.dispose_requested, slot.name.clone())
                }
                _ => return,
            }
        };
        warn!("plugin `{name}` apply was cancelled; restored to pending");
        if dispose {
            self.host.dispose_fiber_sync(self.fiber);
        } else {
            let effects = self.host.lock().effects.remove(&self.fiber);
            drop(effects);
        }
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
            effects: HashMap::new(),
            next_fiber: 1,
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
        for (index, slot) in self.fibers.iter().enumerate().skip(FIRST_CHILD) {
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

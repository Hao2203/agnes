//! 类型化事件服务：`emit`、`bail` 与异步 `waterfall`。
//!
//! `Events` 是独立的 root service，不依赖 `ServiceHost`；监听器由注册方通过
//! `Ctx::effect` 登记清理 guard，插件卸载时自动移除。

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;

use crate::ctx::Ctx;
use crate::effect::EffectGuard;

static NEXT_EVENTS_STAMP: AtomicU64 = AtomicU64::new(1);

/// `waterfall` 的 continuation：`await` 它把控制权交给下游与默认实现。
pub type Next<'a, R> = Pin<Box<dyn Future<Output = R> + Send + 'a>>;

/// 把 future 装箱成 [`Next`]，便于监听器返回。
pub(crate) fn next<'a, R>(future: impl Future<Output = R> + Send + 'a) -> Next<'a, R> {
    Box::pin(future)
}

pub type EmitListener<E> = dyn Fn(&E) + Send + Sync;
pub type BailListener<E, R> = dyn Fn(&E) -> Option<R> + Send + Sync;
pub type WaterfallListener<E, R> = dyn for<'a> Fn(&'a E, Next<'a, R>) -> Next<'a, R> + Send + Sync;

struct EmitMarker<E>(PhantomData<E>);
struct BailMarker<E, R>(PhantomData<(E, R)>);
struct WaterfallMarker<E, R>(PhantomData<(E, R)>);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ListenerSetId {
    pub(crate) name: &'static str,
    pub(crate) kind: TypeId,
}

/// 广播观察事件。
pub struct EventKey<E> {
    name: &'static str,
    marker: PhantomData<fn(&E)>,
}

impl<E> Clone for EventKey<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for EventKey<E> {}

impl<E> fmt::Debug for EventKey<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventKey")
            .field("name", &self.name)
            .finish()
    }
}

impl<E: 'static> EventKey<E> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            marker: PhantomData,
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn set_id(&self) -> ListenerSetId {
        ListenerSetId {
            name: self.name,
            kind: TypeId::of::<EmitMarker<E>>(),
        }
    }
}

/// 首个非 `None` 返回值胜出的决策事件。
pub struct BailKey<E, R> {
    name: &'static str,
    marker: PhantomData<fn(&E, &R)>,
}

impl<E, R> Clone for BailKey<E, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E, R> Copy for BailKey<E, R> {}

impl<E, R> fmt::Debug for BailKey<E, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BailKey").field("name", &self.name).finish()
    }
}

impl<E: 'static, R: 'static> BailKey<E, R> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            marker: PhantomData,
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn set_id(&self) -> ListenerSetId {
        ListenerSetId {
            name: self.name,
            kind: TypeId::of::<BailMarker<E, R>>(),
        }
    }
}

/// 环绕中间件：监听器可以包装 `next().await` 的返回值，也可以直接短路。
pub struct WaterfallKey<E, R> {
    name: &'static str,
    marker: PhantomData<fn(&E, &R)>,
}

impl<E, R> Clone for WaterfallKey<E, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E, R> Copy for WaterfallKey<E, R> {}

impl<E, R> fmt::Debug for WaterfallKey<E, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaterfallKey")
            .field("name", &self.name)
            .finish()
    }
}

impl<E: 'static, R: 'static> WaterfallKey<E, R> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            marker: PhantomData,
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn set_id(&self) -> ListenerSetId {
        ListenerSetId {
            name: self.name,
            kind: TypeId::of::<WaterfallMarker<E, R>>(),
        }
    }
}

pub(crate) type RemoveListenerFn = fn(&mut Box<dyn Any + Send + Sync>, usize) -> bool;

/// 一次监听器注册的句柄；可以用 [`Events::off`] 提前撤销。
#[derive(Clone, Copy)]
pub struct Listener {
    pub(crate) events: u64,
    pub(crate) set: ListenerSetId,
    pub(crate) id: usize,
    pub(crate) remove: RemoveListenerFn,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listener")
            .field("events", &self.events)
            .field("id", &self.id)
            .finish()
    }
}

impl PartialEq for Listener {
    fn eq(&self, other: &Self) -> bool {
        self.events == other.events && self.id == other.id
    }
}

impl Eq for Listener {}

impl Hash for Listener {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.events.hash(state);
        self.id.hash(state);
    }
}

pub(crate) struct Slot<F: ?Sized> {
    pub(crate) id: usize,
    pub(crate) callback: Arc<F>,
}

impl<F: ?Sized> Clone for Slot<F> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            callback: Arc::clone(&self.callback),
        }
    }
}

pub(crate) struct ListenerSet<F: ?Sized> {
    slots: Vec<Slot<F>>,
}

impl<F: ?Sized> Default for ListenerSet<F> {
    fn default() -> Self {
        Self { slots: Vec::new() }
    }
}

impl<F: ?Sized> ListenerSet<F> {
    pub(crate) fn push(&mut self, slot: Slot<F>) {
        self.slots.push(slot);
    }

    pub(crate) fn prepend(&mut self, slot: Slot<F>) {
        self.slots.insert(0, slot);
    }

    pub(crate) fn remove(&mut self, id: usize) -> bool {
        match self.slots.iter().position(|slot| slot.id == id) {
            Some(index) => {
                self.slots.remove(index);
                true
            }
            None => false,
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<Slot<F>> {
        self.slots.clone()
    }
}

fn remove_from_set<F: ?Sized + Send + Sync + 'static>(
    boxed: &mut Box<dyn Any + Send + Sync>,
    id: usize,
) -> bool {
    let any: &mut dyn Any = &mut **boxed;
    match any.downcast_mut::<ListenerSet<F>>() {
        Some(set) => set.remove(id),
        None => false,
    }
}

#[derive(Default)]
struct EventInner {
    listeners: HashMap<ListenerSetId, Box<dyn Any + Send + Sync>>,
    next_listener: usize,
}

impl EventInner {
    fn listener_set_mut<F: ?Sized + Send + Sync + 'static>(
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

    fn listener_set<F: ?Sized + Send + Sync + 'static>(
        &self,
        id: ListenerSetId,
    ) -> Option<&ListenerSet<F>> {
        let any: &dyn Any = &**self.listeners.get(&id)?;
        any.downcast_ref::<ListenerSet<F>>()
    }
}

/// 独立的事件服务；根服务，插件通过 `Deps` 注入。
pub struct Events {
    stamp: u64,
    inner: Mutex<EventInner>,
}

impl Default for Events {
    fn default() -> Self {
        Self {
            stamp: NEXT_EVENTS_STAMP.fetch_add(1, Ordering::Relaxed),
            inner: Mutex::new(EventInner::default()),
        }
    }
}

impl Events {
    fn lock(&self) -> MutexGuard<'_, EventInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 注册广播监听器，追加在末尾；插件卸载时通过 effect guard 自动移除。
    pub async fn on<E: 'static>(
        self: &Arc<Self>,
        ctx: &Ctx<'_>,
        key: EventKey<E>,
        listener: impl Fn(&E) + Send + Sync + 'static,
    ) -> Listener {
        let callback: Arc<EmitListener<E>> = Arc::new(listener);
        self.register_effect(
            ctx,
            key.set_id(),
            callback,
            false,
            remove_from_set::<EmitListener<E>>,
        )
        .await
    }

    /// 注册广播监听器，插到最前面。
    pub async fn prepend<E: 'static>(
        self: &Arc<Self>,
        ctx: &Ctx<'_>,
        key: EventKey<E>,
        listener: impl Fn(&E) + Send + Sync + 'static,
    ) -> Listener {
        let callback: Arc<EmitListener<E>> = Arc::new(listener);
        self.register_effect(
            ctx,
            key.set_id(),
            callback,
            true,
            remove_from_set::<EmitListener<E>>,
        )
        .await
    }

    /// 注册决策监听器，追加在末尾。
    pub async fn on_bail<E: 'static, R: 'static>(
        self: &Arc<Self>,
        ctx: &Ctx<'_>,
        key: BailKey<E, R>,
        listener: impl Fn(&E) -> Option<R> + Send + Sync + 'static,
    ) -> Listener {
        let callback: Arc<BailListener<E, R>> = Arc::new(listener);
        self.register_effect(
            ctx,
            key.set_id(),
            callback,
            false,
            remove_from_set::<BailListener<E, R>>,
        )
        .await
    }

    /// 注册决策监听器，插到最前面。
    pub async fn prepend_bail<E: 'static, R: 'static>(
        self: &Arc<Self>,
        ctx: &Ctx<'_>,
        key: BailKey<E, R>,
        listener: impl Fn(&E) -> Option<R> + Send + Sync + 'static,
    ) -> Listener {
        let callback: Arc<BailListener<E, R>> = Arc::new(listener);
        self.register_effect(
            ctx,
            key.set_id(),
            callback,
            true,
            remove_from_set::<BailListener<E, R>>,
        )
        .await
    }

    /// 注册 waterfall 监听器，追加在末尾。
    pub async fn on_waterfall<E: Send + Sync + 'static, R: 'static>(
        self: &Arc<Self>,
        ctx: &Ctx<'_>,
        key: WaterfallKey<E, R>,
        listener: impl for<'a> Fn(&'a E, Next<'a, R>) -> Next<'a, R> + Send + Sync + 'static,
    ) -> Listener {
        let callback: Arc<WaterfallListener<E, R>> = Arc::new(listener);
        self.register_effect(
            ctx,
            key.set_id(),
            callback,
            false,
            remove_from_set::<WaterfallListener<E, R>>,
        )
        .await
    }

    /// 注册 waterfall 监听器，插到最前面。
    pub async fn prepend_waterfall<E: Send + Sync + 'static, R: 'static>(
        self: &Arc<Self>,
        ctx: &Ctx<'_>,
        key: WaterfallKey<E, R>,
        listener: impl for<'a> Fn(&'a E, Next<'a, R>) -> Next<'a, R> + Send + Sync + 'static,
    ) -> Listener {
        let callback: Arc<WaterfallListener<E, R>> = Arc::new(listener);
        self.register_effect(
            ctx,
            key.set_id(),
            callback,
            true,
            remove_from_set::<WaterfallListener<E, R>>,
        )
        .await
    }

    async fn register_effect<F: ?Sized + Send + Sync + 'static>(
        self: &Arc<Self>,
        ctx: &Ctx<'_>,
        set_id: ListenerSetId,
        callback: Arc<F>,
        prepend: bool,
        remove: RemoveListenerFn,
    ) -> Listener {
        let events = Arc::clone(self);
        ctx.effect(async move {
            let listener = events.register(set_id, callback, prepend, remove);
            let guard = ListenerGuard {
                events: Arc::clone(&events),
                listener,
            };
            (listener, guard)
        })
        .await
    }

    fn register<F: ?Sized + Send + Sync + 'static>(
        &self,
        set_id: ListenerSetId,
        callback: Arc<F>,
        prepend: bool,
        remove: RemoveListenerFn,
    ) -> Listener {
        let mut inner = self.lock();
        let id = inner.next_listener;
        inner.next_listener += 1;
        let slot = Slot { id, callback };
        let set = inner.listener_set_mut::<F>(set_id);
        if prepend {
            set.prepend(slot);
        } else {
            set.push(slot);
        }
        Listener {
            events: self.stamp,
            set: set_id,
            id,
            remove,
        }
    }

    fn remove(&self, listener: Listener) -> bool {
        if listener.events != self.stamp {
            return false;
        }
        let mut inner = self.lock();
        let Some(boxed) = inner.listeners.get_mut(&listener.set) else {
            return false;
        };
        (listener.remove)(boxed, listener.id)
    }

    /// 提前撤销一个监听器；已撤销或不属于本服务时返回 `false`。
    pub fn off(&self, listener: Listener) -> bool {
        self.remove(listener)
    }

    /// 同步广播；返回值被忽略。
    pub fn emit<E: 'static>(&self, key: EventKey<E>, event: &E) {
        let slots = self.snapshot::<EmitListener<E>>(key.set_id());
        for slot in slots {
            (slot.callback)(event);
        }
    }

    /// 决策：第一个返回 `Some` 的监听器胜出。
    pub fn bail<E: 'static, R: 'static>(&self, key: BailKey<E, R>, event: &E) -> Option<R> {
        let slots = self.snapshot::<BailListener<E, R>>(key.set_id());
        for slot in slots {
            if let Some(value) = (slot.callback)(event) {
                return Some(value);
            }
        }
        None
    }

    /// 环绕中间件；监听器按注册顺序由外向内包装。
    pub async fn waterfall<E: Send + Sync + 'static, R: 'static>(
        &self,
        key: WaterfallKey<E, R>,
        event: &E,
        default: impl FnOnce() -> R + Send,
    ) -> R {
        let slots = self.snapshot::<WaterfallListener<E, R>>(key.set_id());
        run_waterfall(&slots, event, 0, next(async move { default() })).await
    }

    fn snapshot<F: ?Sized + Send + Sync + 'static>(&self, set_id: ListenerSetId) -> Vec<Slot<F>> {
        self.lock()
            .listener_set::<F>(set_id)
            .map(|set| set.snapshot())
            .unwrap_or_default()
    }
}

fn run_waterfall<'e, E: Send + Sync + 'static, R: 'static>(
    slots: &'e [Slot<WaterfallListener<E, R>>],
    event: &'e E,
    index: usize,
    default: Next<'e, R>,
) -> Next<'e, R> {
    next(async move {
        let Some(slot) = slots.get(index) else {
            return default.await;
        };
        let slot = slot.clone();
        let continuation: Next<'e, R> = run_waterfall(slots, event, index + 1, default);
        (slot.callback)(event, continuation).await
    })
}

struct ListenerGuard {
    events: Arc<Events>,
    listener: Listener,
}

#[async_trait]
impl EffectGuard for ListenerGuard {
    async fn close(&mut self) {
        self.events.remove(self.listener);
    }
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        self.events.remove(self.listener);
    }
}

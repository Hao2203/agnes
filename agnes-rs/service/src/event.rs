//! 类型化事件：`emit`、`bail` 与异步 `waterfall`。
//!
//! 监听器在分发时拿到属于自己 fiber 的 `&Ctx`，因此可以安全地注册/查找
//! 服务；分发只持有注册表的快照，不持锁调用用户代码。

use std::any::{Any, TypeId};
use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use crate::effect::Effect;
use crate::host::{Ctx, Inner, ServiceHost};
use crate::plugin::Fiber;

/// `waterfall` 的 continuation：`await` 它把控制权交给下游与默认实现。
pub type Next<'a, R> = Pin<Box<dyn Future<Output = R> + Send + 'a>>;

/// 把 future 装箱成 [`Next`]，便于监听器返回。
pub(crate) fn next<'a, R>(future: impl Future<Output = R> + Send + 'a) -> Next<'a, R> {
    Box::pin(future)
}

pub type EmitListener<E> = dyn for<'a, 'b, 'h> Fn(&'a Ctx<'h>, &'b E) + Send + Sync;
pub type BailListener<E, R> = dyn for<'a, 'b, 'h> Fn(&'a Ctx<'h>, &'b E) -> Option<R> + Send + Sync;
pub type WaterfallListener<E, R> =
    dyn for<'a, 'h> Fn(&'a Ctx<'h>, &'a E, Next<'a, R>) -> Next<'a, R> + Send + Sync;

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

/// 一次监听器注册的句柄；可以用 [`Ctx::off`] 提前撤销。
#[derive(Clone, Copy)]
pub struct Listener {
    pub(crate) host: u64,
    pub(crate) set: ListenerSetId,
    pub(crate) id: usize,
    pub(crate) remove: RemoveListenerFn,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listener")
            .field("host", &self.host)
            .field("id", &self.id)
            .finish()
    }
}

impl PartialEq for Listener {
    fn eq(&self, other: &Self) -> bool {
        self.host == other.host && self.id == other.id
    }
}

impl Eq for Listener {}

impl Hash for Listener {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.host.hash(state);
        self.id.hash(state);
    }
}

pub(crate) struct Slot<F: ?Sized> {
    pub(crate) id: usize,
    pub(crate) owner: Fiber,
    pub(crate) callback: Arc<F>,
}

impl<F: ?Sized> Clone for Slot<F> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            owner: self.owner,
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

pub(crate) fn remove_listener(inner: &mut Inner, listener: Listener) -> bool {
    if listener.host != inner.stamp {
        return false;
    }
    let Some(boxed) = inner.listeners.get_mut(&listener.set) else {
        return false;
    };
    (listener.remove)(boxed, listener.id)
}

pub(crate) fn on<E: 'static>(
    host: &ServiceHost,
    owner: Fiber,
    key: EventKey<E>,
    listener: impl for<'a, 'b, 'h> Fn(&'a Ctx<'h>, &'b E) + Send + Sync + 'static,
    prepend: bool,
) -> Listener {
    let mut inner = host.lock();
    let set_id = key.set_id();
    let id = inner.next_listener;
    inner.next_listener += 1;
    let callback: Arc<EmitListener<E>> = Arc::new(listener);
    let slot = Slot {
        id,
        owner,
        callback,
    };
    let set = inner.listener_set_mut::<EmitListener<E>>(set_id);
    if prepend {
        set.prepend(slot);
    } else {
        set.push(slot);
    }
    let token = Listener {
        host: inner.stamp,
        set: set_id,
        id,
        remove: remove_from_set::<EmitListener<E>>,
    };
    inner
        .effects
        .entry(owner)
        .or_default()
        .push(Effect::RemoveListener(token));
    token
}

pub(crate) fn on_bail<E: 'static, R: 'static>(
    host: &ServiceHost,
    owner: Fiber,
    key: BailKey<E, R>,
    listener: impl for<'a, 'b, 'h> Fn(&'a Ctx<'h>, &'b E) -> Option<R> + Send + Sync + 'static,
    prepend: bool,
) -> Listener {
    let mut inner = host.lock();
    let set_id = key.set_id();
    let id = inner.next_listener;
    inner.next_listener += 1;
    let callback: Arc<BailListener<E, R>> = Arc::new(listener);
    let slot = Slot {
        id,
        owner,
        callback,
    };
    let set = inner.listener_set_mut::<BailListener<E, R>>(set_id);
    if prepend {
        set.prepend(slot);
    } else {
        set.push(slot);
    }
    let token = Listener {
        host: inner.stamp,
        set: set_id,
        id,
        remove: remove_from_set::<BailListener<E, R>>,
    };
    inner
        .effects
        .entry(owner)
        .or_default()
        .push(Effect::RemoveListener(token));
    token
}

pub(crate) fn on_waterfall<E: Send + Sync + 'static, R: 'static>(
    host: &ServiceHost,
    owner: Fiber,
    key: WaterfallKey<E, R>,
    listener: impl for<'a, 'h> Fn(&'a Ctx<'h>, &'a E, Next<'a, R>) -> Next<'a, R>
    + Send
    + Sync
    + 'static,
    prepend: bool,
) -> Listener {
    let mut inner = host.lock();
    let set_id = key.set_id();
    let id = inner.next_listener;
    inner.next_listener += 1;
    let callback: Arc<WaterfallListener<E, R>> = Arc::new(listener);
    let slot = Slot {
        id,
        owner,
        callback,
    };
    let set = inner.listener_set_mut::<WaterfallListener<E, R>>(set_id);
    if prepend {
        set.prepend(slot);
    } else {
        set.push(slot);
    }
    let token = Listener {
        host: inner.stamp,
        set: set_id,
        id,
        remove: remove_from_set::<WaterfallListener<E, R>>,
    };
    inner
        .effects
        .entry(owner)
        .or_default()
        .push(Effect::RemoveListener(token));
    token
}

pub(crate) fn emit<E: 'static>(host: &ServiceHost, key: EventKey<E>, event: &E) {
    let slots = {
        let inner = host.lock();
        inner
            .listener_set::<EmitListener<E>>(key.set_id())
            .map(|set| set.snapshot())
            .unwrap_or_default()
    };
    for slot in slots {
        let ctx = Ctx::new(host, slot.owner);
        (slot.callback)(&ctx, event);
    }
}

pub(crate) fn bail<E: 'static, R: 'static>(
    host: &ServiceHost,
    key: BailKey<E, R>,
    event: &E,
) -> Option<R> {
    let slots = {
        let inner = host.lock();
        inner
            .listener_set::<BailListener<E, R>>(key.set_id())
            .map(|set| set.snapshot())
            .unwrap_or_default()
    };
    for slot in slots {
        let ctx = Ctx::new(host, slot.owner);
        if let Some(value) = (slot.callback)(&ctx, event) {
            return Some(value);
        }
    }
    None
}

pub(crate) async fn waterfall<E: Send + Sync + 'static, R: 'static>(
    host: &ServiceHost,
    key: WaterfallKey<E, R>,
    event: &E,
    default: impl FnOnce() -> R + Send,
) -> R {
    let slots = {
        let inner = host.lock();
        inner
            .listener_set::<WaterfallListener<E, R>>(key.set_id())
            .map(|set| set.snapshot())
            .unwrap_or_default()
    };
    run_waterfall(&slots, host, event, 0, next(async move { default() })).await
}

fn run_waterfall<'e, E: Send + Sync + 'static, R: 'static>(
    slots: &'e [Slot<WaterfallListener<E, R>>],
    host: &'e ServiceHost,
    event: &'e E,
    index: usize,
    default: Next<'e, R>,
) -> Next<'e, R> {
    next(async move {
        let Some(slot) = slots.get(index) else {
            return default.await;
        };
        let slot = slot.clone();
        let continuation: Next<'e, R> = run_waterfall(slots, host, event, index + 1, default);
        let ctx = Ctx::new(host, slot.owner);
        (slot.callback)(&ctx, event, continuation).await
    })
}

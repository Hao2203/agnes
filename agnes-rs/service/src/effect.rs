//! 可逆注册：每个 effect 都带一个同步 disposer，卸载时逆序回卷。
//!
//! 需要 `await` 的资源收尾放在 [`Plugin::shutdown`](crate::Plugin::shutdown)
//! 里；effect 的 disposer 必须是同步的，因为 `Drop` 兜底时不能 await。

use std::any::TypeId;

use crate::event::Listener;
use crate::plugin::Fiber;

/// 同步清理函数。异步收尾请实现 [`Plugin::shutdown`](crate::Plugin::shutdown)。
pub type Disposer = Box<dyn FnOnce() + Send + 'static>;

pub(crate) enum Effect {
    RemoveService { id: TypeId, provider: Fiber },
    RemoveListener(Listener),
    DisposeChild(Fiber),
    Custom(Disposer),
}

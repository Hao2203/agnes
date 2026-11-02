//! 异步 effect：资源由 guard 持有，正常卸载时 `close().await`，取消/Drop 时靠
//! guard 的 `Drop` 兜底。
//!
//! [`Effects`] 是每个 fiber 私有的 LIFO 容器：`close()` 和 `Drop` 都从尾部
//! 弹出 guard，保证后登记的资源先释放。

use async_trait::async_trait;

/// 一个可逆 effect 的 guard。
///
/// 正常卸载时宿主调用 [`EffectGuard::close`]；future 被取消或宿主 `Drop` 时
/// 没有机会 `await`，只依赖实现者的 `Drop` 做同步清理。`close` 与 `Drop`
/// 都必须是幂等的。
#[async_trait]
pub trait EffectGuard: Send + 'static {
    /// 正常卸载时的异步收尾。
    async fn close(&mut self);
}

/// 每个 fiber 一个的 LIFO effect 容器。
#[derive(Default)]
pub(crate) struct Effects {
    guards: Vec<Box<dyn EffectGuard>>,
}

impl Effects {
    pub(crate) fn push(&mut self, guard: Box<dyn EffectGuard>) {
        self.guards.push(guard);
    }

    /// 按后进先出顺序 `await` 每个 guard 的异步收尾。
    pub(crate) async fn close(&mut self) {
        while let Some(mut guard) = self.guards.pop() {
            guard.close().await;
        }
    }
}

impl Drop for Effects {
    fn drop(&mut self) {
        // Vec 默认从前到后 drop；显式从尾部弹出，保证 LIFO。
        while let Some(guard) = self.guards.pop() {
            drop(guard);
        }
    }
}

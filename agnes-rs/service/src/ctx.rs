//! 插件作用域：只提供 `effect`。
//!
//! 服务查询/提供、事件、子插件挂载都是独立 service；`Ctx` 只负责把资源
//! 登记到当前 fiber 的 LIFO effect 容器里。

use std::future::Future;

use crate::effect::EffectGuard;
use crate::host::ServiceHost;
use crate::plugin::Fiber;

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

    /// 异步登记一个 effect；`setup` 产出的值返回给调用者，guard 交给宿主。
    ///
    /// 正常卸载时按 LIFO 调用 guard 的 `close().await`；future 被取消或宿主
    /// `Drop` 时只依赖 guard 的 `Drop` 做同步兜底。
    pub async fn effect<T, G>(&self, setup: impl Future<Output = (T, G)> + Send) -> T
    where
        T: Send,
        G: EffectGuard,
    {
        let (value, guard) = setup.await;
        self.host
            .lock()
            .effects
            .entry(self.fiber)
            .or_default()
            .push(Box::new(guard));
        value
    }

    pub(crate) fn fiber(&self) -> Fiber {
        self.fiber
    }

    pub(crate) fn host(&self) -> &'h ServiceHost {
        self.host
    }
}

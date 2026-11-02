//! 子插件挂载 capability。

use std::sync::Weak;

use async_trait::async_trait;

use crate::ctx::Ctx;
use crate::effect::EffectGuard;
use crate::host::{ServiceHost, Shared};
use crate::plugin::{Fiber, Plugin};

/// 子挂载 capability；根服务，插件通过 `Deps` 注入。
#[derive(Clone, Copy, Debug, Default)]
pub struct Mounts;

impl Mounts {
    /// 在当前 fiber 下挂载子插件；父 fiber 的 effect 容器负责销毁。
    pub async fn mount<P: Plugin>(&self, ctx: &Ctx<'_>, plugin: P) -> Fiber {
        let child = ctx
            .effect(async move {
                let child = ctx.host().mount_child(ctx.fiber(), plugin);
                let guard = ChildGuard {
                    host: ctx.host().downgrade(),
                    fiber: child,
                };
                (child, guard)
            })
            .await;
        ctx.host().settle_if_idle().await;
        child
    }
}

struct ChildGuard {
    host: Weak<Shared>,
    fiber: Fiber,
}

#[async_trait]
impl EffectGuard for ChildGuard {
    async fn close(&mut self) {
        if let Some(shared) = self.host.upgrade() {
            let host = ServiceHost::from_shared(shared);
            host.unmount_inner(self.fiber).await;
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(shared) = self.host.upgrade() {
            let host = ServiceHost::from_shared(shared);
            host.dispose_fiber_sync(self.fiber);
        }
    }
}

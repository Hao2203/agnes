//! 服务表与类型级依赖。
//!
//! 依赖不是手写列表：`Plugin::Deps` 的类型决定依赖集合。
//! `Arc<T>` 是硬依赖，`Option<Arc<T>>` 是可选依赖，元组把多个依赖组合起来。

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::ctx::Ctx;
use crate::error::ServiceError;
use crate::plugin::Fiber;

/// 一个服务依赖。`id` 是身份，`name` 只进日志与诊断。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServiceDep {
    id: TypeId,
    name: &'static str,
}

impl ServiceDep {
    pub fn of<T: ?Sized + 'static>() -> Self {
        Self {
            id: TypeId::of::<T>(),
            name: type_name::<T>(),
        }
    }

    pub fn id(&self) -> TypeId {
        self.id
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

impl fmt::Debug for ServiceDep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ServiceDep").field(&self.name).finish()
    }
}

/// 插件依赖的类型级描述。
///
/// 实现者不是插件作者，而是服务类型本身：`Arc<T>`、`Option<Arc<T>>`、`()`
/// 与元组。内核在挂载时调用 [`ServiceParam::deps`]，在激活前调用
/// [`ServiceParam::resolve`]，因此插件不需要手写 `inject` 列表。
pub trait ServiceParam: Send + Sync + 'static {
    type Output: Send;

    fn deps_into(deps: &mut Vec<ServiceDep>);

    fn deps() -> Vec<ServiceDep> {
        let mut deps = Vec::new();
        Self::deps_into(&mut deps);
        deps
    }

    fn resolve(ctx: &Ctx<'_>) -> Option<Self::Output>;
}

impl ServiceParam for () {
    type Output = ();

    fn deps_into(_deps: &mut Vec<ServiceDep>) {}

    fn resolve(_ctx: &Ctx<'_>) -> Option<Self::Output> {
        Some(())
    }
}

impl<T: ?Sized + Send + Sync + 'static> ServiceParam for Arc<T> {
    type Output = Arc<T>;

    fn deps_into(deps: &mut Vec<ServiceDep>) {
        deps.push(ServiceDep::of::<T>());
    }

    fn resolve(ctx: &Ctx<'_>) -> Option<Self::Output> {
        ctx.get::<T>()
    }
}

/// 可选依赖：不参与 `Pending` 判定，解析结果可以是 `None`。
impl<T: ?Sized + Send + Sync + 'static> ServiceParam for Option<Arc<T>> {
    type Output = Option<Arc<T>>;

    fn deps_into(_deps: &mut Vec<ServiceDep>) {}

    fn resolve(ctx: &Ctx<'_>) -> Option<Self::Output> {
        Some(ctx.get::<T>())
    }
}

macro_rules! impl_service_param_tuple {
    ($($name:ident),+) => {
        impl<$($name: ServiceParam),+> ServiceParam for ($($name,)+) {
            type Output = ($($name::Output,)+);

            fn deps_into(deps: &mut Vec<ServiceDep>) {
                $($name::deps_into(deps);)+
            }

            fn resolve(ctx: &Ctx<'_>) -> Option<Self::Output> {
                Some(($($name::resolve(ctx)?,)+))
            }
        }
    };
}

impl_service_param_tuple!(A, B);
impl_service_param_tuple!(A, B, C);
impl_service_param_tuple!(A, B, C, D);
impl_service_param_tuple!(A, B, C, D, E);
impl_service_param_tuple!(A, B, C, D, E, F);
impl_service_param_tuple!(A, B, C, D, E, F, G);
impl_service_param_tuple!(A, B, C, D, E, F, G, H);

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ServiceInfo {
    pub type_name: &'static str,
    pub provider: Fiber,
    pub provider_name: String,
}

pub(crate) struct ServiceEntry {
    pub(crate) provider: Fiber,
    pub(crate) type_name: &'static str,
    pub(crate) value: Box<dyn Any + Send + Sync>,
}

#[derive(Default)]
pub(crate) struct ServiceTable {
    entries: HashMap<TypeId, ServiceEntry>,
}

impl ServiceTable {
    pub(crate) fn provide<T>(&mut self, provider: Fiber, value: Arc<T>) -> Result<(), ServiceError>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let id = TypeId::of::<T>();
        if let Some(existing) = self.entries.get(&id) {
            return Err(ServiceError::ServiceTaken {
                type_name: existing.type_name,
            });
        }
        self.entries.insert(
            id,
            ServiceEntry {
                provider,
                type_name: type_name::<T>(),
                value: Box::new(value),
            },
        );
        Ok(())
    }

    pub(crate) fn get<T>(&self) -> Option<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.entries
            .get(&TypeId::of::<T>())?
            .value
            .downcast_ref::<Arc<T>>()
            .cloned()
    }

    pub(crate) fn provider(&self, id: TypeId) -> Option<Fiber> {
        self.entries.get(&id).map(|entry| entry.provider)
    }

    pub(crate) fn remove(&mut self, id: TypeId, provider: Fiber) -> bool {
        if self
            .entries
            .get(&id)
            .is_some_and(|entry| entry.provider == provider)
        {
            self.entries.remove(&id);
            true
        } else {
            false
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (&TypeId, &ServiceEntry)> {
        self.entries.iter()
    }
}

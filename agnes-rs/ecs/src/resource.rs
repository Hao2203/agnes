use std::any::Any;
use std::cell::{Ref, RefCell, RefMut};
use std::ops::{Deref, DerefMut};

/// Marker trait for global values stored in the [`World`](crate::World).
///
/// Every `Any + Send + Sync` type implements it — unlike [`Component`], which
/// is opt-in. That is safe here because the resource API surface has no
/// bundle-shaped sibling: `insert_resource` always stores exactly the value
/// it is given, so a tuple cannot silently masquerade as several resources.
/// Resource storage is separate from component storage, so the same type can
/// be both.
///
/// [`Component`]: crate::Component
pub trait Resource: Any + Send + Sync {}

impl<T: Any + Send + Sync> Resource for T {}

pub(crate) trait ErasedResource: Any + Send {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

pub(crate) struct ResourceCell<T: Resource> {
    value: RefCell<T>,
}

impl<T: Resource> ResourceCell<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            value: RefCell::new(value),
        }
    }

    pub(crate) fn borrow(&self) -> Ref<'_, T> {
        self.value.borrow()
    }

    pub(crate) fn borrow_mut(&self) -> RefMut<'_, T> {
        self.value.borrow_mut()
    }

    pub(crate) fn replace(&self, value: T) -> T {
        self.value.replace(value)
    }

    pub(crate) fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: Resource> ErasedResource for ResourceCell<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Shared access to a resource.
pub struct Res<'w, T: Resource> {
    value: Ref<'w, T>,
}

impl<'w, T: Resource> Res<'w, T> {
    pub(crate) fn new(value: Ref<'w, T>) -> Self {
        Self { value }
    }
}

impl<T: Resource> Deref for Res<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

/// Exclusive access to a resource.
pub struct ResMut<'w, T: Resource> {
    value: RefMut<'w, T>,
}

impl<'w, T: Resource> ResMut<'w, T> {
    pub(crate) fn new(value: RefMut<'w, T>) -> Self {
        Self { value }
    }
}

impl<T: Resource> Deref for ResMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T: Resource> DerefMut for ResMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

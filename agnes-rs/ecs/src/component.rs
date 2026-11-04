use std::any::Any;
use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;

use crate::entity::Entity;

/// Marker trait for values that can be attached to an [`Entity`].
///
/// Every `Any + Send + Sync` type implements it. Resources use a separate
/// type namespace, so the same type can be both a component and a resource.
pub trait Component: Any + Send + Sync {}

impl<T: Any + Send + Sync> Component for T {}

pub(crate) trait ErasedStorage: Any + Send {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn remove_entity(&mut self, entity: Entity);
}

pub(crate) struct Storage<T: Component> {
    values: HashMap<Entity, RefCell<T>>,
}

impl<T: Component> Storage<T> {
    pub(crate) fn new() -> Self {
        Self {
            values: HashMap::new(),
        }
    }

    pub(crate) fn insert(&mut self, entity: Entity, value: T) {
        self.values.insert(entity, RefCell::new(value));
    }

    pub(crate) fn remove(&mut self, entity: Entity) -> Option<T> {
        self.values.remove(&entity).map(RefCell::into_inner)
    }

    pub(crate) fn contains(&self, entity: Entity) -> bool {
        self.values.contains_key(&entity)
    }

    pub(crate) fn get(&self, entity: Entity) -> Option<Ref<'_, T>> {
        self.values.get(&entity).map(RefCell::borrow)
    }

    pub(crate) fn get_mut(&self, entity: Entity) -> Option<RefMut<'_, T>> {
        self.values.get(&entity).map(RefCell::borrow_mut)
    }
}

impl<T: Component> ErasedStorage for Storage<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn remove_entity(&mut self, entity: Entity) {
        self.values.remove(&entity);
    }
}

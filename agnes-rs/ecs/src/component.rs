use std::any::Any;
use std::cell::{Ref, RefCell, RefMut};

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

/// Marks an unused entry in [`Storage::sparse`].
const NO_DENSE_SLOT: u32 = u32::MAX;

/// Sparse-set storage for one component type.
///
/// `sparse` maps an entity index to its slot in `dense`, which keeps the
/// component value next to the owning entity so a stale generation cannot
/// observe another entity's value. Lookups are a slice index plus a
/// generation check instead of a hash.
///
/// Dense ordering is an implementation detail: [`Storage::remove`] uses
/// `swap_remove`, so no iteration order is guaranteed.
pub(crate) struct Storage<T: Component> {
    sparse: Vec<u32>,
    dense: Vec<(Entity, RefCell<T>)>,
}

impl<T: Component> Storage<T> {
    pub(crate) fn new() -> Self {
        Self {
            sparse: Vec::new(),
            dense: Vec::new(),
        }
    }

    pub(crate) fn insert(&mut self, entity: Entity, value: T) {
        if let Some(position) = self.dense_position(entity) {
            self.dense[position].1 = RefCell::new(value);
            return;
        }
        let index = entity_index(entity);
        if index >= self.sparse.len() {
            self.sparse.resize(index + 1, NO_DENSE_SLOT);
        }
        let position = u32::try_from(self.dense.len()).expect("dense length fits in u32");
        self.sparse[index] = position;
        self.dense.push((entity, RefCell::new(value)));
    }

    pub(crate) fn remove(&mut self, entity: Entity) -> Option<T> {
        let position = self.dense_position(entity)?;
        let (_, cell) = self.dense.swap_remove(position);
        if position < self.dense.len() {
            let (moved, _) = &self.dense[position];
            self.sparse[entity_index(*moved)] =
                u32::try_from(position).expect("dense length fits in u32");
        }
        self.sparse[entity_index(entity)] = NO_DENSE_SLOT;
        Some(cell.into_inner())
    }

    pub(crate) fn contains(&self, entity: Entity) -> bool {
        self.dense_position(entity).is_some()
    }

    pub(crate) fn get(&self, entity: Entity) -> Option<Ref<'_, T>> {
        let position = self.dense_position(entity)?;
        Some(self.dense[position].1.borrow())
    }

    pub(crate) fn get_mut(&self, entity: Entity) -> Option<RefMut<'_, T>> {
        let position = self.dense_position(entity)?;
        Some(self.dense[position].1.borrow_mut())
    }

    fn dense_position(&self, entity: Entity) -> Option<usize> {
        let slot = *self.sparse.get(entity_index(entity))?;
        if slot == NO_DENSE_SLOT {
            return None;
        }
        let position = usize::try_from(slot).expect("dense slot fits in usize");
        let (stored, _) = self.dense.get(position)?;
        (*stored == entity).then_some(position)
    }
}

fn entity_index(entity: Entity) -> usize {
    usize::try_from(entity.index()).expect("u32 entity index fits in usize")
}

impl<T: Component> ErasedStorage for Storage<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn remove_entity(&mut self, entity: Entity) {
        self.remove(entity);
    }
}

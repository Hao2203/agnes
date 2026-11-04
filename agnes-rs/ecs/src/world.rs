use std::any::TypeId;
use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::fmt;

use crate::component::{Component, ErasedStorage, Storage};
use crate::entity::{Entity, EntityAllocator};
use crate::message::{ErasedMessageQueue, Message, MessageQueue, ReaderId};
use crate::resource::{ErasedResource, Resource, ResourceCell};

/// The container for all ECS state.
///
/// Components, resources, and messages are stored behind interior mutability
/// so multiple system parameters can be extracted at the same time. Structural
/// changes are deferred through [`Commands`](crate::Commands) and applied with
/// `&mut World`.
pub struct World {
    entities: RefCell<EntityAllocator>,
    components: HashMap<TypeId, Box<dyn ErasedStorage>>,
    resources: HashMap<TypeId, Box<dyn ErasedResource>>,
    messages: HashMap<TypeId, Box<dyn ErasedMessageQueue>>,
}

impl World {
    /// Creates an empty world.
    pub fn new() -> Self {
        Self {
            entities: RefCell::new(EntityAllocator::default()),
            components: HashMap::new(),
            resources: HashMap::new(),
            messages: HashMap::new(),
        }
    }

    /// Spawns an entity with one component.
    pub fn spawn<T: Component>(&mut self, component: T) -> Entity {
        let entity = self.entities.get_mut().spawn();
        self.insert(entity, component);
        entity
    }

    /// Spawns an entity without components.
    pub fn spawn_empty(&mut self) -> Entity {
        self.entities.get_mut().spawn()
    }

    /// Reserves an entity id that only becomes alive when confirmed.
    ///
    /// Used by [`Commands`](crate::Commands) so entities created during a
    /// system are not visible until the command queue is applied.
    pub(crate) fn reserve_entity(&self) -> Entity {
        self.entities.borrow_mut().reserve()
    }

    pub(crate) fn confirm_entity(&mut self, entity: Entity) -> bool {
        self.entities.get_mut().confirm(entity).is_some()
    }

    /// Whether the entity is alive and has the same generation.
    pub fn is_alive(&self, entity: Entity) -> bool {
        self.entities.borrow().is_alive(entity)
    }

    /// Despawns an entity and removes all of its components.
    ///
    /// Returns `false` when the handle is stale or already despawned.
    pub fn despawn(&mut self, entity: Entity) -> bool {
        if !self.entities.get_mut().despawn(entity) {
            return false;
        }
        for storage in self.components.values_mut() {
            storage.remove_entity(entity);
        }
        true
    }

    /// Inserts or replaces a component on a live entity.
    ///
    /// Returns `false` when the entity is not alive.
    ///
    /// # Panics
    ///
    /// Panics if an internal storage entry has a mismatched `TypeId`, which
    /// indicates a bug in the world itself.
    pub fn insert<T: Component>(&mut self, entity: Entity, component: T) -> bool {
        if !self.is_alive(entity) {
            return false;
        }
        let type_id = TypeId::of::<T>();
        let storage = self
            .components
            .entry(type_id)
            .or_insert_with(|| Box::new(Storage::<T>::new()));
        let storage = storage
            .as_any_mut()
            .downcast_mut::<Storage<T>>()
            .expect("component storage matches its TypeId");
        storage.insert(entity, component);
        true
    }

    /// Removes a component from an entity.
    pub fn remove<T: Component>(&mut self, entity: Entity) -> Option<T> {
        let storage = self.components.get_mut(&TypeId::of::<T>())?;
        storage
            .as_any_mut()
            .downcast_mut::<Storage<T>>()?
            .remove(entity)
    }

    /// Borrows a component value.
    ///
    /// # Panics
    ///
    /// Panics if the same component value is already mutably borrowed.
    pub fn get<T: Component>(&self, entity: Entity) -> Option<Ref<'_, T>> {
        let storage = self.components.get(&TypeId::of::<T>())?;
        storage.as_any().downcast_ref::<Storage<T>>()?.get(entity)
    }

    /// Mutably borrows a component value.
    ///
    /// # Panics
    ///
    /// Panics if the same component value is already borrowed.
    pub fn get_mut<T: Component>(&self, entity: Entity) -> Option<RefMut<'_, T>> {
        let storage = self.components.get(&TypeId::of::<T>())?;
        storage
            .as_any()
            .downcast_ref::<Storage<T>>()?
            .get_mut(entity)
    }

    /// Whether the entity has a component of type `T`.
    pub fn contains<T: Component>(&self, entity: Entity) -> bool {
        let Some(storage) = self.components.get(&TypeId::of::<T>()) else {
            return false;
        };
        storage
            .as_any()
            .downcast_ref::<Storage<T>>()
            .is_some_and(|storage| storage.contains(entity))
    }

    /// Inserts a resource, returning the previous value if one existed.
    ///
    /// # Panics
    ///
    /// Panics if an internal resource entry has a mismatched `TypeId`, which
    /// indicates a bug in the world itself.
    pub fn insert_resource<T: Resource>(&mut self, value: T) -> Option<T> {
        match self.resources.entry(TypeId::of::<T>()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let cell = entry
                    .get_mut()
                    .as_any_mut()
                    .downcast_mut::<ResourceCell<T>>()
                    .expect("resource cell matches its TypeId");
                Some(cell.replace(value))
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Box::new(ResourceCell::new(value)));
                None
            }
        }
    }

    /// Borrows a resource.
    ///
    /// # Panics
    ///
    /// Panics if the resource is already mutably borrowed.
    pub fn resource<T: Resource>(&self) -> Option<Ref<'_, T>> {
        let cell = self.resource_cell::<T>()?;
        Some(cell.borrow())
    }

    /// Mutably borrows a resource.
    ///
    /// # Panics
    ///
    /// Panics if the resource is already borrowed.
    pub fn resource_mut<T: Resource>(&self) -> Option<RefMut<'_, T>> {
        let cell = self.resource_cell::<T>()?;
        Some(cell.borrow_mut())
    }

    /// Whether a resource of type `T` exists.
    pub fn contains_resource<T: Resource>(&self) -> bool {
        self.resources.contains_key(&TypeId::of::<T>())
    }

    /// Removes and returns a resource.
    pub fn remove_resource<T: Resource>(&mut self) -> Option<T> {
        let resource = self.resources.remove(&TypeId::of::<T>())?;
        resource
            .into_any()
            .downcast::<ResourceCell<T>>()
            .ok()
            .map(|cell| cell.into_inner())
    }

    pub(crate) fn alive_entities(&self) -> Vec<Entity> {
        self.entities.borrow().alive_entities()
    }

    pub(crate) fn alive_len(&self) -> usize {
        self.entities.borrow().alive_len()
    }

    /// Registers the message queue for `T` and returns it.
    pub(crate) fn register_message<T: Message>(&mut self) -> &mut MessageQueue<T> {
        let type_id = TypeId::of::<T>();
        self.messages
            .entry(type_id)
            .or_insert_with(|| Box::new(MessageQueue::<T>::new()));
        self.messages
            .get_mut(&type_id)
            .expect("message queue was just inserted")
            .as_any_mut()
            .downcast_mut::<MessageQueue<T>>()
            .expect("message queue matches its TypeId")
    }

    /// Registers a reader for `T` and returns its identity.
    pub(crate) fn register_message_reader<T: Message>(&mut self) -> ReaderId {
        self.register_message::<T>().register_reader()
    }

    pub(crate) fn message_queue<T: Message>(&self) -> Option<&MessageQueue<T>> {
        self.messages
            .get(&TypeId::of::<T>())?
            .as_any()
            .downcast_ref::<MessageQueue<T>>()
    }

    /// Queues a host message for the next schedule run.
    ///
    /// Host messages are published after all systems are initialized and before
    /// any system runs, so readers created during this run see them. Messages
    /// written by systems through [`MessageWriter`](crate::MessageWriter) are
    /// published at the end of the run and become visible on the next run.
    pub fn send_message<T: Message>(&mut self, message: T) {
        self.register_message::<T>().queue(message);
    }

    /// Publishes host messages at the start of a schedule run.
    pub(crate) fn flush_incoming_messages(&mut self) {
        for queue in self.messages.values_mut() {
            queue.flush_incoming();
        }
    }

    /// Publishes all messages queued by system writers.
    pub(crate) fn flush_messages(&mut self) {
        for queue in self.messages.values_mut() {
            queue.flush();
        }
    }

    /// Whether any message type has work that a future run can observe.
    ///
    /// This drives the automatic multi-run drain: a writer publishes at the end
    /// of one run, and readers consume it on the next.
    pub fn has_unread_messages(&self) -> bool {
        self.messages.values().any(|queue| queue.has_unread())
    }

    #[cfg(test)]
    pub(crate) fn published_message_len<T: Message>(&self) -> usize {
        self.message_queue::<T>()
            .map_or(0, MessageQueue::published_len)
    }

    fn resource_cell<T: Resource>(&self) -> Option<&ResourceCell<T>> {
        self.resources
            .get(&TypeId::of::<T>())?
            .as_any()
            .downcast_ref::<ResourceCell<T>>()
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for World {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("World")
            .field("entities", &self.alive_len())
            .field("component_types", &self.components.len())
            .field("resources", &self.resources.len())
            .field("message_types", &self.messages.len())
            .finish()
    }
}

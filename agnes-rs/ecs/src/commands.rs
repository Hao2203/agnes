use crate::component::Component;
use crate::entity::Entity;
use crate::world::World;

type QueuedCommand = Box<dyn FnOnce(&mut World) + Send + 'static>;

/// Per-system buffer of deferred structural changes.
#[doc(hidden)]
#[derive(Default)]
pub struct CommandQueue {
    commands: Vec<QueuedCommand>,
}

impl CommandQueue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, command: QueuedCommand) {
        self.commands.push(command);
    }

    pub(crate) fn apply(&mut self, world: &mut World) {
        for command in self.commands.drain(..) {
            command(world);
        }
    }
}

/// Deferred structural access to the world.
///
/// Commands are queued while a system runs and applied after it returns, once
/// all query and resource borrows have been released.
pub struct Commands<'w, 's> {
    world: &'w World,
    queue: &'s mut CommandQueue,
}

impl<'w, 's> Commands<'w, 's> {
    pub(crate) fn new(world: &'w World, queue: &'s mut CommandQueue) -> Self {
        Self { world, queue }
    }

    /// Reserves an entity and queues its component insertion.
    ///
    /// The returned entity is not visible to queries until commands are
    /// applied.
    pub fn spawn<T: Component>(&mut self, component: T) -> Entity {
        let entity = self.world.reserve_entity();
        self.queue.push(Box::new(move |world| {
            if !world.confirm_entity(entity) {
                log::warn!("skipping spawn of stale entity {entity:?}");
                return;
            }
            if !world.insert(entity, component) {
                log::warn!("spawned entity {entity:?} disappeared before insertion");
            }
        }));
        entity
    }

    /// Reserves an entity without components.
    pub fn spawn_empty(&mut self) -> Entity {
        let entity = self.world.reserve_entity();
        self.queue.push(Box::new(move |world| {
            if !world.confirm_entity(entity) {
                log::warn!("skipping spawn of stale entity {entity:?}");
            }
        }));
        entity
    }

    /// Queues commands for an existing or reserved entity.
    pub fn entity(&mut self, entity: Entity) -> EntityCommands<'_> {
        EntityCommands {
            entity,
            queue: self.queue,
        }
    }
}

/// Deferred commands targeting one entity.
pub struct EntityCommands<'s> {
    entity: Entity,
    queue: &'s mut CommandQueue,
}

impl<'s> EntityCommands<'s> {
    /// The target entity.
    pub fn id(&self) -> Entity {
        self.entity
    }

    /// Queues a component insertion.
    pub fn insert<T: Component>(self, component: T) -> Self {
        let entity = self.entity;
        self.queue.push(Box::new(move |world| {
            if !world.is_alive(entity) {
                log::warn!("skipping insertion for stale entity {entity:?}");
                return;
            }
            world.insert(entity, component);
        }));
        self
    }

    /// Queues a component removal.
    pub fn remove<T: Component>(self) -> Self {
        let entity = self.entity;
        self.queue.push(Box::new(move |world| {
            world.remove::<T>(entity);
        }));
        self
    }

    /// Queues a despawn.
    pub fn despawn(self) {
        let entity = self.entity;
        self.queue.push(Box::new(move |world| {
            if !world.despawn(entity) {
                log::warn!("skipping despawn of stale entity {entity:?}");
            }
        }));
    }
}

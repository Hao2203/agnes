use std::fmt;

/// A lightweight entity identifier with a generation counter.
///
/// The index is reused after a despawn, while the generation is bumped so a
/// stale handle can never observe components belonging to the new entity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Entity {
    index: u32,
    generation: u32,
}

impl Entity {
    /// A placeholder handle. It never refers to a live entity.
    pub const PLACEHOLDER: Self = Self {
        index: u32::MAX,
        generation: u32::MAX,
    };

    /// The slot index.
    pub fn index(self) -> u32 {
        self.index
    }

    /// The generation of the slot.
    pub fn generation(self) -> u32 {
        self.generation
    }

    pub(crate) fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }
}

impl fmt::Debug for Entity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Entity({}:{})", self.index, self.generation)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    Free,
    Reserved,
    Alive,
    Retired,
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    generation: u32,
    state: SlotState,
}

#[derive(Debug, Default)]
pub(crate) struct EntityAllocator {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl EntityAllocator {
    pub(crate) fn spawn(&mut self) -> Entity {
        let entity = self.reserve();
        self.confirm(entity)
            .expect("a freshly reserved entity is always confirmable");
        entity
    }

    pub(crate) fn reserve(&mut self) -> Entity {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                let index = u32::try_from(self.slots.len()).expect("entity slot count fits in u32");
                self.slots.push(Slot {
                    generation: 0,
                    state: SlotState::Free,
                });
                index
            }
        };
        let slot = &mut self.slots[slot_index(index)];
        debug_assert_eq!(slot.state, SlotState::Free);
        slot.state = SlotState::Reserved;
        Entity::new(index, slot.generation)
    }

    pub(crate) fn confirm(&mut self, entity: Entity) -> Option<Entity> {
        let slot = self.slots.get_mut(usize::try_from(entity.index).ok()?)?;
        if slot.state != SlotState::Reserved || slot.generation != entity.generation {
            return None;
        }
        slot.state = SlotState::Alive;
        Some(entity)
    }

    pub(crate) fn despawn(&mut self, entity: Entity) -> bool {
        let Ok(index) = usize::try_from(entity.index) else {
            return false;
        };
        let Some(slot) = self.slots.get_mut(index) else {
            return false;
        };
        if slot.state != SlotState::Alive || slot.generation != entity.generation {
            return false;
        }
        let next_generation = slot.generation.checked_add(1);
        match next_generation {
            Some(generation) => {
                slot.generation = generation;
                slot.state = SlotState::Free;
                self.free.push(entity.index);
            }
            None => {
                slot.state = SlotState::Retired;
            }
        }
        true
    }

    pub(crate) fn is_alive(&self, entity: Entity) -> bool {
        let Ok(index) = usize::try_from(entity.index) else {
            return false;
        };
        let Some(slot) = self.slots.get(index) else {
            return false;
        };
        slot.state == SlotState::Alive && slot.generation == entity.generation
    }

    pub(crate) fn iter_alive(&self) -> impl Iterator<Item = Entity> + '_ {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            if slot.state != SlotState::Alive {
                return None;
            }
            let index = u32::try_from(index).expect("entity slot index fits in u32");
            Some(Entity::new(index, slot.generation))
        })
    }

    pub(crate) fn alive_entities(&self) -> Vec<Entity> {
        let mut entities = Vec::with_capacity(self.slots.len());
        entities.extend(self.iter_alive());
        entities
    }

    pub(crate) fn alive_len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.state == SlotState::Alive)
            .count()
    }
}

fn slot_index(index: u32) -> usize {
    usize::try_from(index).expect("u32 entity index fits in usize")
}

#[cfg(test)]
mod tests {
    use super::EntityAllocator;

    #[test]
    fn reusing_an_index_bumps_the_generation() {
        let mut allocator = EntityAllocator::default();
        let first = allocator.spawn();
        assert!(allocator.despawn(first));
        let second = allocator.spawn();
        assert_eq!(first.index(), second.index());
        assert_ne!(first.generation(), second.generation());
        assert!(!allocator.is_alive(first));
        assert!(allocator.is_alive(second));
    }

    #[test]
    fn a_reserved_entity_is_not_alive_until_confirmed() {
        let mut allocator = EntityAllocator::default();
        let entity = allocator.reserve();
        assert!(!allocator.is_alive(entity));
        assert!(allocator.alive_entities().is_empty());
        allocator.confirm(entity).expect("reserved entity confirms");
        assert!(allocator.is_alive(entity));
    }

    #[test]
    fn despawning_an_unknown_entity_is_false() {
        let mut allocator = EntityAllocator::default();
        let entity = allocator.spawn();
        allocator.despawn(entity);
        assert!(!allocator.despawn(entity));
    }
}

use std::cell::{Ref, RefMut};
use std::marker::PhantomData;

use crate::access::{Access, AccessMode, AccessSet};
use crate::component::Component;
use crate::entity::Entity;
use crate::world::World;

/// Describes the data a [`Query`] fetches for each matching entity.
pub trait QueryData {
    /// The item yielded for one entity.
    type Item<'w>;

    /// Whether the entity has all required components.
    fn matches(world: &World, entity: Entity) -> bool;

    /// Fetches the item for one entity.
    fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>>;

    /// The component accesses required by this data.
    fn access() -> AccessSet;
}

impl<T: Component> QueryData for &T {
    type Item<'w> = Ref<'w, T>;

    fn matches(world: &World, entity: Entity) -> bool {
        world.contains::<T>(entity)
    }

    fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
        world.get::<T>(entity)
    }

    fn access() -> AccessSet {
        let mut set = AccessSet::new();
        set.push(Access::component::<T>(AccessMode::Read));
        set
    }
}

impl<T: Component> QueryData for &mut T {
    type Item<'w> = RefMut<'w, T>;

    fn matches(world: &World, entity: Entity) -> bool {
        world.contains::<T>(entity)
    }

    fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
        world.get_mut::<T>(entity)
    }

    fn access() -> AccessSet {
        let mut set = AccessSet::new();
        set.push(Access::component::<T>(AccessMode::Write));
        set
    }
}

impl QueryData for Entity {
    type Item<'w> = Entity;

    fn matches(_world: &World, _entity: Entity) -> bool {
        true
    }

    fn fetch<'w>(_world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
        Some(entity)
    }

    fn access() -> AccessSet {
        AccessSet::new()
    }
}

macro_rules! impl_query_data_tuple {
    ($($name:ident),+) => {
        impl<$($name: QueryData),+> QueryData for ($($name,)+) {
            type Item<'w> = ($($name::Item<'w>,)+);

            fn matches(world: &World, entity: Entity) -> bool {
                true $(&& $name::matches(world, entity))+
            }

            fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
                Some(($($name::fetch(world, entity)?,)+))
            }

            fn access() -> AccessSet {
                let mut set = AccessSet::new();
                $(set.extend($name::access());)+
                set
            }
        }
    };
}

impl_query_data_tuple!(A, B);
impl_query_data_tuple!(A, B, C);
impl_query_data_tuple!(A, B, C, D);
impl_query_data_tuple!(A, B, C, D, E);
impl_query_data_tuple!(A, B, C, D, E, F);
impl_query_data_tuple!(A, B, C, D, E, F, G);
impl_query_data_tuple!(A, B, C, D, E, F, G, H);

/// Restricts a query to entities that also have component `T`.
pub struct With<T>(PhantomData<T>);

/// Restricts a query to entities that do not have component `T`.
pub struct Without<T>(PhantomData<T>);

/// A filter applied on top of a query's data.
pub trait QueryFilter {
    /// Whether the entity passes the filter.
    fn matches(world: &World, entity: Entity) -> bool;
}

impl QueryFilter for () {
    fn matches(_world: &World, _entity: Entity) -> bool {
        true
    }
}

impl<T: Component> QueryFilter for With<T> {
    fn matches(world: &World, entity: Entity) -> bool {
        world.contains::<T>(entity)
    }
}

impl<T: Component> QueryFilter for Without<T> {
    fn matches(world: &World, entity: Entity) -> bool {
        !world.contains::<T>(entity)
    }
}

/// A typed view over entities matching a component shape and filter.
///
/// Query items are [`Ref`] or [`RefMut`] guards rather than raw references.
/// They dereference to the component, so ordinary field access works:
///
/// ```ignore
/// for mut agent in query.iter_mut() {
///     agent.active = true;
/// }
/// ```
pub struct Query<'w, D: QueryData, F: QueryFilter = ()> {
    world: &'w World,
    marker: PhantomData<(D, F)>,
}

impl<'w, D: QueryData, F: QueryFilter> Clone for Query<'w, D, F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'w, D: QueryData, F: QueryFilter> Copy for Query<'w, D, F> {}

impl<'w, D: QueryData, F: QueryFilter> Query<'w, D, F> {
    pub(crate) fn new(world: &'w World) -> Self {
        Self {
            world,
            marker: PhantomData,
        }
    }

    /// Iterates all matching entities.
    ///
    /// # Panics
    ///
    /// Iterating a mutable query can panic if two mutable borrows of the same
    /// component value on the same entity are alive at once.
    pub fn iter(&self) -> QueryIter<'w, D, F> {
        let entities = self.world.alive_entities();
        let mut matched = Vec::with_capacity(entities.len());
        for entity in entities {
            if D::matches(self.world, entity) && F::matches(self.world, entity) {
                matched.push(entity);
            }
        }
        QueryIter {
            world: self.world,
            entities: matched.into_iter(),
            marker: PhantomData,
        }
    }

    /// Iterates all matching entities through a mutable query.
    ///
    /// # Panics
    ///
    /// Panics if two mutable borrows of the same component value on the same
    /// entity are alive at once.
    pub fn iter_mut(&mut self) -> QueryIter<'w, D, F> {
        self.iter()
    }

    /// Fetches the item for one entity.
    pub fn get(&self, entity: Entity) -> Option<D::Item<'w>> {
        if !D::matches(self.world, entity) || !F::matches(self.world, entity) {
            return None;
        }
        D::fetch(self.world, entity)
    }

    /// Mutably fetches the item for one entity.
    ///
    /// # Panics
    ///
    /// Panics if the same component value on this entity is already borrowed.
    pub fn get_mut(&mut self, entity: Entity) -> Option<D::Item<'w>> {
        self.get(entity)
    }

    /// The number of matching entities.
    pub fn count(&self) -> usize {
        self.iter().count()
    }

    /// Whether no entity matches.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
}

/// The iterator returned by [`Query::iter`] and [`Query::iter_mut`].
pub struct QueryIter<'w, D: QueryData, F: QueryFilter> {
    world: &'w World,
    entities: std::vec::IntoIter<Entity>,
    marker: PhantomData<(D, F)>,
}

impl<'w, D: QueryData, F: QueryFilter> Iterator for QueryIter<'w, D, F> {
    type Item = D::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        for entity in self.entities.by_ref() {
            if let Some(item) = D::fetch(self.world, entity) {
                return Some(item);
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entities.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use crate::world::World;

    use super::{Query, With, Without};

    #[derive(Debug, PartialEq, Eq)]
    struct Name(&'static str);

    #[derive(Debug, PartialEq, Eq)]
    struct Active(bool);

    #[derive(Debug, PartialEq, Eq)]
    struct Running;

    fn query<'w>(world: &'w World) -> Query<'w, &'static Name> {
        Query::new(world)
    }

    #[test]
    fn query_reads_matching_components() {
        let mut world = World::new();
        world.spawn(Name("a"));
        world.spawn(Name("b"));
        world.spawn(Active(true));

        let query = query(&world);
        let names: Vec<&'static str> = query.iter().map(|name| name.0).collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn query_mutates_only_matching_entities() {
        let mut world = World::new();
        let first = world.spawn(Active(false));
        world.spawn(Name("only-name"));

        {
            let mut query = Query::<&'static mut Active>::new(&world);
            for mut active in query.iter_mut() {
                active.0 = true;
            }
        }

        assert!(world.get::<Active>(first).expect("active exists").0);
    }

    #[test]
    fn filters_restrict_entities() {
        let mut world = World::new();
        let running = world.spawn(Name("running"));
        world.insert(running, Running);
        world.spawn(Name("idle"));

        let query = Query::<&'static Name, With<Running>>::new(&world);
        let running: Vec<&'static str> = query.iter().map(|name| name.0).collect();
        assert_eq!(running, ["running"]);

        let query = Query::<&'static Name, Without<Running>>::new(&world);
        let idle: Vec<&'static str> = query.iter().map(|name| name.0).collect();
        assert_eq!(idle, ["idle"]);
    }

    #[test]
    fn entity_can_be_part_of_the_item() {
        let mut world = World::new();
        let entity = world.spawn(Name("a"));

        let query = Query::<(crate::Entity, &'static Name)>::new(&world);
        let matches: Vec<_> = query.iter().map(|(found, name)| (found, name.0)).collect();
        assert_eq!(matches, [(entity, "a")]);
    }
}

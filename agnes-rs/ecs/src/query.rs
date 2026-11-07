use std::any::TypeId;
use std::cell::{Ref, RefMut};
use std::marker::PhantomData;

use crate::access::{Access, AccessMode};
use crate::component::{Component, ErasedStorage};
use crate::entity::Entity;
use crate::world::World;

/// Describes the data a [`Query`] fetches for each matching entity.
pub trait QueryData {
    /// The item yielded for one entity.
    type Item<'w>;

    /// Fetches the item for one entity.
    ///
    /// Returns `None` when the entity does not carry every required
    /// component.
    fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>>;

    /// The component accesses required by this data.
    ///
    /// This is a declaration-time value: callers collect it once when a
    /// system is registered, so returning an iterator keeps the per-component
    /// declarations allocation-free.
    fn access() -> impl Iterator<Item = Access> {
        std::iter::empty()
    }

    /// The components every match must have, in declaration order.
    ///
    /// Iteration is driven by the smallest of these component storages, so
    /// only entities carrying at least one of them are visited. A query with
    /// no required component (only [`Entity`] or [`Option`] data) falls back
    /// to walking live entities.
    fn required_components() -> impl Iterator<Item = TypeId> {
        std::iter::empty()
    }
}

impl<T: Component> QueryData for &T {
    type Item<'w> = Ref<'w, T>;

    fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
        world.get::<T>(entity)
    }

    fn access() -> impl Iterator<Item = Access> {
        std::iter::once(Access::component::<T>(AccessMode::Read))
    }

    fn required_components() -> impl Iterator<Item = TypeId> {
        std::iter::once(TypeId::of::<T>())
    }
}

impl<T: Component> QueryData for &mut T {
    type Item<'w> = RefMut<'w, T>;

    fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
        world.get_mut::<T>(entity)
    }

    fn access() -> impl Iterator<Item = Access> {
        std::iter::once(Access::component::<T>(AccessMode::Write))
    }

    fn required_components() -> impl Iterator<Item = TypeId> {
        std::iter::once(TypeId::of::<T>())
    }
}

/// `Option<&T>` matches every entity and yields `None` when `T` is absent.
///
/// Unlike `&T` it never filters an entity out of the query. The component is
/// still read access: combining `Option<&T>` with `&mut T` in one system is
/// rejected at registration time.
impl<T: Component> QueryData for Option<&T> {
    type Item<'w> = Option<Ref<'w, T>>;

    fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
        Some(world.get::<T>(entity))
    }

    fn access() -> impl Iterator<Item = Access> {
        std::iter::once(Access::component::<T>(AccessMode::Read))
    }
}

/// `Option<&mut T>` matches every entity and yields `None` when `T` is absent.
///
/// The component is still write access, so two parameters writing `T` in one
/// system are rejected at registration time even behind `Option`.
impl<T: Component> QueryData for Option<&mut T> {
    type Item<'w> = Option<RefMut<'w, T>>;

    fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
        Some(world.get_mut::<T>(entity))
    }

    fn access() -> impl Iterator<Item = Access> {
        std::iter::once(Access::component::<T>(AccessMode::Write))
    }
}

/// `Entity` matches every live entity, with or without components.
///
/// It is not a component-existence filter: to target a kind of entity, query
/// its component (`Query<&T>`) or add a [`With`] filter.
impl QueryData for Entity {
    type Item<'w> = Entity;

    fn fetch<'w>(_world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
        Some(entity)
    }
}

macro_rules! impl_query_data_tuple {
    ($($name:ident),+) => {
        impl<$($name: QueryData),+> QueryData for ($($name,)+) {
            type Item<'w> = ($($name::Item<'w>,)+);

            fn fetch<'w>(world: &'w World, entity: Entity) -> Option<Self::Item<'w>> {
                Some(($($name::fetch(world, entity)?,)+))
            }

            fn access() -> impl Iterator<Item = Access> {
                std::iter::empty() $(.chain($name::access()))+
            }

            fn required_components() -> impl Iterator<Item = TypeId> {
                std::iter::empty() $(.chain($name::required_components()))+
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

    /// The components every match must have, in declaration order.
    ///
    /// Filters such as [`With`] can drive iteration without declaring read
    /// access to the component; filters that only exclude entities
    /// ([`Without`]) yield nothing.
    fn required_components() -> impl Iterator<Item = TypeId> {
        std::iter::empty()
    }
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

    fn required_components() -> impl Iterator<Item = TypeId> {
        std::iter::once(TypeId::of::<T>())
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
///
/// Iteration is driven by the smallest component storage among the query's
/// required components — every `&T`, `&mut T`, and `With<T>` — so it visits
/// only entities that carry at least one of them. Queries without a required
/// component ([`Entity`], [`Option`] data, or a lone [`Without`]) fall back to
/// walking live entities.
///
/// Matches are yielded in storage order, which is insertion order modified by
/// removals; no ordering is guaranteed.
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
    /// The iterator allocates nothing. When the query requires a component,
    /// it walks that component's dense storage, so the cost scales with the
    /// number of entities carrying the component rather than with all entity
    /// slots. Queries without a required component walk live entities
    /// instead. Filters are applied lazily, one component-storage lookup per
    /// candidate.
    ///
    /// Entities reserved through [`Commands`](crate::Commands) from the loop
    /// body are not alive yet and are skipped. Structural changes cannot
    /// happen mid-iteration: they require `&mut World` and are applied after
    /// the system returns.
    ///
    /// # Panics
    ///
    /// Iterating a mutable query can panic if two mutable borrows of the same
    /// component value on the same entity are alive at once.
    pub fn iter(&self) -> QueryIter<'w, D, F> {
        QueryIter {
            world: self.world,
            source: select_source(
                self.world,
                D::required_components().chain(F::required_components()),
            ),
            cursor: 0,
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
        if !F::matches(self.world, entity) {
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

    /// The single matching entity, or an error when zero or several match.
    ///
    /// # Errors
    ///
    /// Returns [`QuerySingleError::NoEntity`] when nothing matches and
    /// [`QuerySingleError::MultipleEntities`] when more than one does.
    pub fn single(&self) -> Result<D::Item<'w>, QuerySingleError> {
        let mut iter = self.iter();
        match (iter.next(), iter.next()) {
            (Some(item), None) => Ok(item),
            (None, _) => Err(QuerySingleError::NoEntity),
            (Some(_), Some(_)) => Err(QuerySingleError::MultipleEntities),
        }
    }

    /// The number of matching entities.
    ///
    /// Uses the same storage-driven walk as [`Query::iter`] and never
    /// allocates.
    pub fn count(&self) -> usize {
        self.iter().count()
    }

    /// Whether no entity matches.
    ///
    /// Uses the same storage-driven walk as [`Query::iter`] and stops at the
    /// first match, so this never allocates.
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

/// Where a [`QueryIter`] gets its candidate entities from.
#[derive(Clone, Copy)]
enum IterSource<'w> {
    /// A required component has no storage, so nothing can match.
    Empty,
    /// Walks one component's dense array; `len` is snapshotted at creation.
    Dense {
        storage: &'w dyn ErasedStorage,
        len: usize,
    },
    /// No required component: walks every live entity.
    Entities,
}

/// The iterator returned by [`Query::iter`] and [`Query::iter_mut`].
///
/// When a required component drives it, the iterator walks that component's
/// dense array, so only entities carrying the component are visited. Otherwise
/// it falls back to walking live entities.
pub struct QueryIter<'w, D: QueryData, F: QueryFilter> {
    world: &'w World,
    source: IterSource<'w>,
    cursor: usize,
    marker: PhantomData<(D, F)>,
}

impl<'w, D: QueryData, F: QueryFilter> Iterator for QueryIter<'w, D, F> {
    type Item = D::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.source {
            IterSource::Empty => None,
            IterSource::Dense { storage, len } => {
                while self.cursor < len {
                    let slot = self.cursor;
                    self.cursor += 1;
                    let Some(entity) = storage.dense_entity_at(slot) else {
                        continue;
                    };
                    if let Some(item) = filtered_fetch::<D, F>(self.world, entity) {
                        return Some(item);
                    }
                }
                None
            }
            IterSource::Entities => {
                while let Some(entity) = self.world.next_alive(&mut self.cursor) {
                    if let Some(item) = filtered_fetch::<D, F>(self.world, entity) {
                        return Some(item);
                    }
                }
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = match self.source {
            IterSource::Empty => 0,
            IterSource::Dense { len, .. } => len.saturating_sub(self.cursor),
            IterSource::Entities => self.world.slot_count().saturating_sub(self.cursor),
        };
        (0, Some(remaining))
    }
}

/// Picks the iteration source for a query.
///
/// The smallest required component storage drives the walk, so the cost
/// tracks the rarest component instead of every entity slot. A missing
/// storage means no entity can carry that component, so the iterator is
/// empty.
fn select_source<'w>(world: &'w World, required: impl Iterator<Item = TypeId>) -> IterSource<'w> {
    let mut smallest: Option<(&'w dyn ErasedStorage, usize)> = None;
    for type_id in required {
        let Some(storage) = world.erased_storage(type_id) else {
            return IterSource::Empty;
        };
        let len = storage.dense_len();
        if smallest.is_none_or(|(_, best)| len < best) {
            smallest = Some((storage, len));
        }
    }
    match smallest {
        Some((storage, len)) => IterSource::Dense { storage, len },
        None => IterSource::Entities,
    }
}

/// Fetches the item for one entity, honoring the query's filter.
///
/// The shared match test behind [`QueryIter::next`], [`Query::count`], and
/// [`Query::is_empty`]: one filter check plus one component fetch per entity.
fn filtered_fetch<'w, D: QueryData, F: QueryFilter>(
    world: &'w World,
    entity: Entity,
) -> Option<D::Item<'w>> {
    if F::matches(world, entity) {
        D::fetch(world, entity)
    } else {
        None
    }
}

/// Why [`Query::single`] did not return exactly one match.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QuerySingleError {
    /// No entity matched the query.
    #[error("no entity matches the query")]
    NoEntity,
    /// More than one entity matched the query.
    #[error("multiple entities match the query")]
    MultipleEntities,
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

    crate::component!(Name, Active, Running);

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
    fn a_with_filter_alone_drives_iteration() {
        let mut world = World::new();
        let first = world.spawn_bundle((Name("a"), Running));
        world.spawn(Name("b"));
        let third = world.spawn(Running);

        let found: Vec<crate::Entity> = Query::<crate::Entity, With<Running>>::new(&world)
            .iter()
            .collect();
        assert_eq!(found, [first, third]);
    }

    #[test]
    fn data_and_filter_both_restrict_matches() {
        let mut world = World::new();
        world.spawn_bundle((Name("tagged"), Running));
        world.spawn(Name("untagged"));
        world.spawn(Running);

        let query = Query::<&'static Name, With<Running>>::new(&world);
        assert_eq!(query.count(), 1);
        assert_eq!(query.single().expect("one match").0, "tagged");
    }

    #[test]
    fn iteration_skips_removed_dense_entries() {
        let mut world = World::new();
        world.spawn(Name("a"));
        let removed = world.spawn(Name("b"));
        world.spawn(Name("c"));

        assert_eq!(world.remove::<Name>(removed), Some(Name("b")));

        let names: Vec<&'static str> = Query::<&'static Name>::new(&world)
            .iter()
            .map(|name| name.0)
            .collect();
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn a_missing_storage_matches_nothing() {
        let world = World::new();
        let query = Query::<&'static Running>::new(&world);

        assert!(query.is_empty());
        assert_eq!(query.count(), 0);
        assert_eq!(query.iter().count(), 0);
    }

    #[test]
    fn entity_can_be_part_of_the_item() {
        let mut world = World::new();
        let entity = world.spawn(Name("a"));

        let query = Query::<(crate::Entity, &'static Name)>::new(&world);
        let matches: Vec<_> = query.iter().map(|(found, name)| (found, name.0)).collect();
        assert_eq!(matches, [(entity, "a")]);
    }

    #[test]
    fn single_returns_the_only_match() {
        let mut world = World::new();
        world.spawn(Name("only"));

        let query = Query::<&'static Name>::new(&world);
        assert_eq!(query.single().expect("one match").0, "only");
    }

    #[test]
    fn single_reports_no_entity() {
        let world = World::new();
        let query = Query::<&'static Name>::new(&world);
        assert!(matches!(
            query.single(),
            Err(crate::QuerySingleError::NoEntity)
        ));
    }

    #[test]
    fn single_reports_multiple_entities() {
        let mut world = World::new();
        world.spawn(Name("a"));
        world.spawn(Name("b"));

        let query = Query::<&'static Name>::new(&world);
        assert!(matches!(
            query.single(),
            Err(crate::QuerySingleError::MultipleEntities)
        ));
    }

    #[test]
    fn count_and_is_empty_match_iteration() {
        let mut world = World::new();
        world.spawn(Name("a"));
        world.spawn(Active(true));
        world.spawn(Name("b"));
        world.spawn(Running);

        let names = Query::<&'static Name>::new(&world);
        let running = Query::<&'static Name, With<Running>>::new(&world);
        let idle = Query::<&'static Name, Without<Running>>::new(&world);
        let none = Query::<&'static Running, Without<Name>>::new(&world);

        assert_eq!(names.count(), 2);
        assert_eq!(names.iter().count(), 2);
        assert!(!names.is_empty());
        assert_eq!(running.count(), 0);
        assert_eq!(idle.count(), 2);
        assert_eq!(none.count(), 1);
        assert!(!none.is_empty());
    }

    #[test]
    fn optional_read_matches_every_entity() {
        let mut world = World::new();
        let with_tag = world.spawn(Name("tagged"));
        world.spawn(Active(true));

        let query = Query::<Option<&'static Name>>::new(&world);
        let items: Vec<Option<&'static str>> = query.iter().map(|name| name.map(|n| n.0)).collect();
        assert_eq!(items, [Some("tagged"), None]);

        assert_eq!(
            query.get(with_tag).expect("with_tag matches").map(|n| n.0),
            Some("tagged")
        );
    }

    #[test]
    fn optional_write_mutates_only_present_components() {
        let mut world = World::new();
        let tagged = world.spawn(Active(false));
        let bare = world.spawn(Name("no-active"));

        {
            let mut query = Query::<Option<&'static mut Active>>::new(&world);
            for mut active in query.iter_mut().flatten() {
                active.0 = true;
            }
        }

        assert!(world.get::<Active>(tagged).expect("tagged has it").0);
        assert!(world.get::<Active>(bare).is_none());
    }
}

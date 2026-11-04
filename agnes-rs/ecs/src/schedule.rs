use crate::access::{Access, AccessSet};
use crate::system::{IntoSystem, System, SystemError};
use crate::world::World;

/// A failure while configuring a schedule.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScheduleError {
    /// A system declares read and write access to the same value.
    #[error("system `{system}` has conflicting access: {first} vs {second}")]
    ConflictingAccess {
        /// The system's type name.
        system: &'static str,
        /// The first conflicting access.
        first: Access,
        /// The second conflicting access.
        second: Access,
    },
}

/// An ordered, single-threaded collection of systems.
///
/// # Examples
///
/// ```
/// use agnes_ecs::{ResMut, Schedule, World};
///
/// #[derive(Default)]
/// struct Count(u32);
///
/// fn increment(mut count: ResMut<Count>) {
///     count.0 += 1;
/// }
///
/// let mut world = World::new();
/// world.insert_resource(Count::default());
///
/// let mut schedule = Schedule::new();
/// schedule.add_system(increment).unwrap();
/// schedule.run(&mut world).unwrap();
///
/// assert_eq!(world.resource::<Count>().unwrap().0, 1);
/// ```
#[derive(Default)]
pub struct Schedule {
    systems: Vec<Box<dyn System>>,
    access: AccessSet,
}

impl Schedule {
    /// Creates an empty schedule.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a system after checking its own access set for conflicts.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::ConflictingAccess`] when the system declares
    /// conflicting accesses, such as `Query<&mut T>` together with `Query<&T>`.
    pub fn add_system<Marker, I>(&mut self, system: I) -> Result<(), ScheduleError>
    where
        I: IntoSystem<Marker>,
    {
        self.add_boxed(system.into_system())
    }

    /// Adds an already boxed system after checking its own access set.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError::ConflictingAccess`] when the system declares
    /// conflicting accesses, such as `Query<&mut T>` together with `Query<&T>`.
    pub fn add_boxed<S: System>(&mut self, system: S) -> Result<(), ScheduleError> {
        if let Some((first, second)) = system.access().conflicts() {
            return Err(ScheduleError::ConflictingAccess {
                system: system.name(),
                first,
                second,
            });
        }
        self.access.merge(system.access());
        self.systems.push(Box::new(system));
        Ok(())
    }

    /// Runs all systems in registration order.
    ///
    /// # Errors
    ///
    /// Returns the first [`SystemError`] encountered. Systems that already
    /// ran keep their effects; later systems are skipped.
    pub fn run(&mut self, world: &mut World) -> Result<(), SystemError> {
        for system in &mut self.systems {
            system.initialize(world);
        }
        world.flush_incoming_messages();
        for system in &mut self.systems {
            system.run(world)?;
        }
        world.flush_messages();
        Ok(())
    }

    /// The number of registered systems.
    pub fn len(&self) -> usize {
        self.systems.len()
    }

    /// Whether the schedule has no systems.
    pub fn is_empty(&self) -> bool {
        self.systems.is_empty()
    }

    /// The merged access set of all registered systems.
    pub fn access(&self) -> &AccessSet {
        &self.access
    }
}

use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::plugin::Plugin;
use crate::resource::Resource;
use crate::runner::{
    AsyncExecutor, CancelToken, Runner, RunnerError, RuntimeEvent, RuntimeSender, TaskSpawner,
};
use crate::schedule::{Schedule, ScheduleError};
use crate::system::{IntoSystem, SystemError};
use crate::world::World;

/// A type-level schedule name.
pub trait ScheduleLabel: 'static {
    /// A human-readable label for diagnostics.
    fn label(&self) -> &'static str;
}

/// The default schedule driven by [`App::update`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Update;

impl ScheduleLabel for Update {
    fn label(&self) -> &'static str {
        "Update"
    }
}

/// The application container: a world, schedules, plugins, and runtime handles.
pub struct App {
    world: World,
    schedules: HashMap<TypeId, Schedule>,
    events: Option<async_channel::Receiver<RuntimeEvent>>,
    sender: RuntimeSender,
    shutdown: CancelToken,
    plugins: HashSet<TypeId>,
}

impl App {
    /// Creates an app with an empty world and an installed async executor.
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        let shutdown = CancelToken::new();
        let sender = RuntimeSender::new(tx);
        let executor = AsyncExecutor::new(sender.clone(), shutdown.clone());
        let mut world = World::new();
        let _ = world.insert_resource(executor);
        Self {
            world,
            schedules: HashMap::new(),
            events: Some(rx),
            sender,
            shutdown,
            plugins: HashSet::new(),
        }
    }

    /// Installs a plugin.
    ///
    /// # Panics
    ///
    /// Panics when a plugin of the same type was already installed.
    pub fn add_plugin<P: Plugin>(&mut self, plugin: P) -> &mut Self {
        let type_id = TypeId::of::<P>();
        assert!(
            self.plugins.insert(type_id),
            "plugin `{}` is already registered",
            plugin.name()
        );
        plugin.build(self);
        self
    }

    /// Adds a system to the labeled schedule.
    ///
    /// # Panics
    ///
    /// Panics when the system declares conflicting access.
    pub fn add_systems<L, Marker, I>(&mut self, label: L, system: I) -> &mut Self
    where
        L: ScheduleLabel,
        I: IntoSystem<Marker>,
    {
        let name = label.label();
        self.try_add_systems_inner(TypeId::of::<L>(), system)
            .unwrap_or_else(|error| panic!("failed to add system to schedule `{name}`: {error}"));
        self
    }

    /// Adds a system to the labeled schedule, returning access conflicts.
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleError`] when the system declares conflicting access.
    pub fn try_add_systems<L, Marker, I>(
        &mut self,
        _label: L,
        system: I,
    ) -> Result<(), ScheduleError>
    where
        L: ScheduleLabel,
        I: IntoSystem<Marker>,
    {
        self.try_add_systems_inner(TypeId::of::<L>(), system)
    }

    fn try_add_systems_inner<Marker, I>(
        &mut self,
        schedule_id: TypeId,
        system: I,
    ) -> Result<(), ScheduleError>
    where
        I: IntoSystem<Marker>,
    {
        let schedule = self.schedules.entry(schedule_id).or_default();
        schedule.add_system(system)
    }

    /// Inserts or replaces a resource.
    pub fn insert_resource<T: Resource>(&mut self, value: T) -> &mut Self {
        let _ = self.world.insert_resource(value);
        self
    }

    /// Inserts `T::default()` when the resource is absent.
    pub fn init_resource<T: Resource + Default>(&mut self) -> &mut Self {
        if !self.world.contains_resource::<T>() {
            let _ = self.world.insert_resource(T::default());
        }
        self
    }

    /// The world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// The world, mutably.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Runs the [`Update`] schedule once.
    ///
    /// # Errors
    ///
    /// Returns [`SystemError`] when a system parameter cannot be prepared.
    pub fn update(&mut self) -> Result<(), SystemError> {
        self.run_schedule::<Update>()
    }

    /// Runs [`Update`] until no unread messages remain, up to a fixed bound.
    ///
    /// A system's message writes are published at the end of its run, so a
    /// message pipeline such as model request → tool request → tool result
    /// needs several runs. The bound prevents a system that writes on every
    /// tick from blocking the host forever.
    ///
    /// # Errors
    ///
    /// Returns [`SystemError`] when a system parameter cannot be prepared.
    pub fn drain(&mut self) -> Result<(), SystemError> {
        const MAX_TICKS: usize = 64;
        for _ in 0..MAX_TICKS {
            self.update()?;
            if !self.world.has_unread_messages() {
                return Ok(());
            }
        }
        log::warn!("schedule still has unread messages after {MAX_TICKS} ticks");
        Ok(())
    }

    /// Runs the schedule with the given label once.
    ///
    /// # Errors
    ///
    /// Returns [`SystemError`] when a system parameter cannot be prepared.
    pub fn run_schedule<L: ScheduleLabel>(&mut self) -> Result<(), SystemError> {
        let Self {
            world, schedules, ..
        } = self;
        let Some(schedule) = schedules.get_mut(&TypeId::of::<L>()) else {
            return Ok(());
        };
        schedule.run(world)
    }

    /// Applies all queued runtime events, then runs [`Update`] once.
    ///
    /// # Errors
    ///
    /// Returns [`SystemError`] when a system parameter cannot be prepared.
    pub fn pump(&mut self) -> Result<(), SystemError> {
        while let Some(events) = self.events.as_ref() {
            match events.try_recv() {
                Ok(event) => event.apply(&mut self.world),
                Err(async_channel::TryRecvError::Empty | async_channel::TryRecvError::Closed) => {
                    break;
                }
            }
        }
        self.drain()
    }

    /// A handle for injecting host messages from another task.
    pub fn runtime_sender(&self) -> RuntimeSender {
        self.sender.clone()
    }

    /// A handle for shutting the runner down.
    pub fn shutdown_handle(&self) -> CancelToken {
        self.shutdown.clone()
    }

    /// Installs the [`TaskSpawner`] used by [`AsyncExecutor::spawn`].
    ///
    /// Without a spawner, submitting async work fails with
    /// [`SpawnError::NoSpawner`](crate::SpawnError::NoSpawner). Ready-made
    /// implementations live in [`adapters`](crate::adapters) behind the
    /// `tokio` and `async-executor` features.
    ///
    /// # Panics
    ///
    /// Panics if the `AsyncExecutor` resource is missing, which cannot happen
    /// for an app created with [`App::new`].
    pub fn set_spawner<S: TaskSpawner>(&mut self, spawner: S) -> &mut Self {
        {
            let mut executor = self
                .world
                .resource_mut::<AsyncExecutor>()
                .expect("App installs an AsyncExecutor resource");
            executor.set_spawner(Arc::new(spawner));
        }
        self
    }

    /// Runs the app on the caller's async runtime.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] when a schedule run fails.
    pub async fn run_async(mut self) -> Result<(), RunnerError> {
        let runner = self.take_runner();
        runner.run(&mut self).await
    }

    /// Takes the event loop out of the app so the caller can drive it.
    ///
    /// # Panics
    ///
    /// Panics if the runner has already been taken.
    pub fn take_runner(&mut self) -> Runner {
        let events = self
            .events
            .take()
            .expect("the runner can only be taken once");
        Runner::new(events, self.shutdown.clone())
    }

    pub(crate) fn apply_runtime_event(&mut self, event: RuntimeEvent) {
        event.apply(&mut self.world);
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

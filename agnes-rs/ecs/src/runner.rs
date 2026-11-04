use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use event_listener::Event;
use futures_lite::future::race;

use crate::app::App;
use crate::message::Message;
use crate::system::SystemError;
use crate::world::World;

pub(crate) type EventFn = Box<dyn FnOnce(&mut World) + Send + 'static>;

pub(crate) struct RuntimeEvent {
    apply: EventFn,
}

impl RuntimeEvent {
    pub(crate) fn new(apply: impl FnOnce(&mut World) + Send + 'static) -> Self {
        Self {
            apply: Box::new(apply),
        }
    }

    pub(crate) fn apply(self, world: &mut World) {
        (self.apply)(world);
    }
}

/// A handle for injecting host events into a running [`App`].
#[derive(Clone)]
pub struct RuntimeSender {
    tx: async_channel::Sender<RuntimeEvent>,
}

/// The runtime event channel is closed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeSendError {
    /// The runner has been dropped.
    #[error("runtime event channel is closed")]
    Closed,
}

impl RuntimeSender {
    pub(crate) fn new(tx: async_channel::Sender<RuntimeEvent>) -> Self {
        Self { tx }
    }

    /// Queues a host message for the next schedule run.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeSendError::Closed`] when the runner is gone.
    pub fn send_message<M: Message>(&self, message: M) -> Result<(), RuntimeSendError> {
        let event = RuntimeEvent::new(move |world| world.send_message(message));
        self.tx.try_send(event).map_err(|error| {
            debug_assert!(
                matches!(error, async_channel::TrySendError::Closed(_)),
                "the runtime channel is unbounded and never reports full"
            );
            RuntimeSendError::Closed
        })
    }
}

/// A cloneable cancellation signal.
///
/// Clones share the flag and the wake-up list, so
/// [`cancel`](CancelToken::cancel) on one clone is observed by every clone.
/// Waiters use a double-check because a notification without a registered
/// listener is lost.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    event: Arc<Event>,
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// Creates a token that starts uncancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels the token and wakes all waiters.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.event.notify(usize::MAX);
    }

    /// Whether the token is already cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Resolves once the token is cancelled.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let listener = self.event.listen();
        if self.is_cancelled() {
            return;
        }
        listener.await;
    }
}

/// Spawns fire-and-forget tasks on the host's executor.
///
/// This is the only extension point required to run the crate under an async
/// runtime. [`adapters`](crate::adapters) provides implementations for Tokio
/// and `async-executor` behind their features; anything else can implement
/// this trait directly.
pub trait TaskSpawner: Send + Sync + 'static {
    /// Spawns a task and returns immediately.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] when the executor cannot accept the task.
    fn spawn(&self, task: Pin<Box<dyn Future<Output = ()> + Send>>) -> Result<(), SpawnError>;
}

/// A task could not be spawned.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpawnError {
    /// No [`TaskSpawner`] was installed with
    /// [`App::set_spawner`](crate::App::set_spawner).
    #[error("no task spawner is installed")]
    NoSpawner,
    /// The installed spawner found no active async runtime.
    #[error("no async runtime is active on the current thread")]
    NoRuntime,
    /// The installed spawner rejected the task.
    #[error("task spawner rejected the task")]
    Spawner(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// A task spawner and message bridge between the ECS and async tasks.
#[derive(Clone)]
pub struct AsyncExecutor {
    sender: RuntimeSender,
    cancellation: CancelToken,
    spawner: Option<Arc<dyn TaskSpawner>>,
}

impl AsyncExecutor {
    pub(crate) fn new(sender: RuntimeSender, cancellation: CancelToken) -> Self {
        Self {
            sender,
            cancellation,
            spawner: None,
        }
    }

    pub(crate) fn set_spawner(&mut self, spawner: Arc<dyn TaskSpawner>) {
        self.spawner = Some(spawner);
    }

    /// The runtime sender used by submitted tasks.
    pub fn sender(&self) -> RuntimeSender {
        self.sender.clone()
    }

    /// Sends a message back into the ECS.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeSendError::Closed`] when the runner is gone.
    pub fn send<M: Message>(&self, message: M) -> Result<(), RuntimeSendError> {
        self.sender.send_message(message)
    }

    /// Submits a fire-and-forget task to the installed [`TaskSpawner`].
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError::NoSpawner`] when no spawner was installed, and
    /// whatever the spawner itself reports otherwise.
    pub fn spawn<F>(&self, future: F) -> Result<(), SpawnError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let Some(spawner) = self.spawner.as_ref() else {
            return Err(SpawnError::NoSpawner);
        };
        spawner.spawn(Box::pin(future))
    }

    /// The application-wide cancellation token.
    pub fn cancellation(&self) -> CancelToken {
        self.cancellation.clone()
    }

    /// Whether the application is shutting down.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

/// The main event loop.
pub struct Runner {
    events: async_channel::Receiver<RuntimeEvent>,
    shutdown: CancelToken,
}

enum Wake {
    Shutdown,
    Event(Option<RuntimeEvent>),
}

impl Runner {
    pub(crate) fn new(
        events: async_channel::Receiver<RuntimeEvent>,
        shutdown: CancelToken,
    ) -> Self {
        Self { events, shutdown }
    }

    /// Runs the schedule once, then once per received host event.
    ///
    /// Shutdown is polled before each event, so a cancelled token wins over
    /// pending events.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] when a schedule run fails.
    pub async fn run(self, app: &mut App) -> Result<(), RunnerError> {
        let Self { events, shutdown } = self;
        app.drain()?;
        loop {
            let wake = race(
                async {
                    shutdown.cancelled().await;
                    Wake::Shutdown
                },
                async {
                    // `async_channel::Receiver::recv` is cancel-safe, so
                    // dropping this future between iterations loses nothing.
                    Wake::Event(events.recv().await.ok())
                },
            )
            .await;
            match wake {
                Wake::Shutdown => break,
                Wake::Event(Some(event)) => {
                    app.apply_runtime_event(event);
                    app.drain()?;
                }
                Wake::Event(None) => break,
            }
        }
        Ok(())
    }
}

/// A runtime failure while driving an [`App`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunnerError {
    /// The async runtime could not be built.
    #[error("failed to build the async runtime: {0}")]
    Runtime(#[from] std::io::Error),
    /// A schedule run failed.
    #[error(transparent)]
    System(#[from] SystemError),
}

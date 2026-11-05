//! Task execution and the message bridge between the ECS and async tasks.

use std::future::Future;
#[cfg(feature = "async-executor")]
use std::sync::Arc;

use agnes_ecs::{CancelToken, Message, RuntimeSendError, RuntimeSender};

/// The async runtime tasks are spawned on, chosen by the plugin that
/// installed the executor.
#[derive(Clone)]
pub(crate) enum Runtime {
    #[cfg(feature = "tokio")]
    Tokio,
    #[cfg(feature = "async-executor")]
    Executor(Arc<async_executor::Executor<'static>>),
}

/// No async runtime is active on this thread.
#[derive(Debug, thiserror::Error)]
#[error("no async runtime is active on the current thread")]
pub struct SpawnError;

/// A task spawner and message bridge between the ECS and async tasks.
///
/// Installed as a resource by [`TokioPlugin`](crate::TokioPlugin) or
/// [`ExecutorPlugin`](crate::ExecutorPlugin). [`spawn`](Self::spawn) hands the
/// future straight to the concrete runtime — no boxed future in between.
#[derive(Clone)]
pub struct AsyncExecutor {
    sender: RuntimeSender,
    cancellation: CancelToken,
    runtime: Runtime,
}

impl AsyncExecutor {
    pub(crate) fn new(sender: RuntimeSender, cancellation: CancelToken, runtime: Runtime) -> Self {
        Self {
            sender,
            cancellation,
            runtime,
        }
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

    /// Submits a fire-and-forget task to the installed runtime.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] when no async runtime is active on the current
    /// thread (the Tokio plugin resolves the handle at spawn time).
    pub fn spawn<F>(&self, future: F) -> Result<(), SpawnError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match &self.runtime {
            #[cfg(feature = "tokio")]
            Runtime::Tokio => {
                let handle = tokio::runtime::Handle::try_current().map_err(|_| SpawnError)?;
                // Dropping the JoinHandle detaches the task.
                drop(handle.spawn(future));
                Ok(())
            }
            #[cfg(feature = "async-executor")]
            Runtime::Executor(executor) => {
                // `async_task::Task` cancels the future when dropped, so
                // detach to keep the task running in the background.
                executor.spawn(future).detach();
                Ok(())
            }
        }
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

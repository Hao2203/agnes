//! `async-executor` adapter.
//!
//! [`ExecutorSpawner`] submits fire-and-forget tasks to a shared
//! [`Executor`]. The host is responsible for driving
//! the executor, typically with
//! `futures_lite::future::block_on(executor.run(...))`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_executor::Executor;

use crate::runner::{SpawnError, TaskSpawner};

/// Spawns tasks on a shared [`Executor`].
#[derive(Clone)]
pub struct ExecutorSpawner {
    executor: Arc<Executor<'static>>,
}

impl ExecutorSpawner {
    /// Creates a spawner for the given executor.
    pub fn new(executor: Arc<Executor<'static>>) -> Self {
        Self { executor }
    }
}

impl TaskSpawner for ExecutorSpawner {
    fn spawn(&self, task: Pin<Box<dyn Future<Output = ()> + Send>>) -> Result<(), SpawnError> {
        // `async_task::Task` cancels the future when dropped, so detach to
        // keep the task running in the background.
        self.executor.spawn(task).detach();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_executor::Executor;
    use futures_lite::future::block_on;

    use super::ExecutorSpawner;
    use crate::runner::TaskSpawner;

    #[test]
    fn executor_spawner_runs_detached_tasks() {
        let executor = Arc::new(Executor::new());
        let spawner = ExecutorSpawner::new(executor.clone());
        let flag = Arc::new(AtomicBool::new(false));
        let task_flag = flag.clone();

        spawner
            .spawn(Box::pin(async move {
                task_flag.store(true, Ordering::SeqCst);
            }))
            .expect("spawn succeeds");

        block_on(executor.run(async {
            while !flag.load(Ordering::SeqCst) {
                futures_lite::future::yield_now().await;
            }
        }));
        assert!(flag.load(Ordering::SeqCst));
    }
}

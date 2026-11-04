//! Tokio adapter.
//!
//! [`TokioSpawner`] resolves the runtime handle at spawn time, so it can be
//! installed before [`run`] builds its runtime.
//! Tasks submitted through it are fire-and-forget: dropping the Tokio
//! `JoinHandle` detaches the task.

use std::future::Future;
use std::pin::Pin;

use tokio::runtime::Handle;

use crate::app::App;
use crate::runner::{RunnerError, SpawnError, TaskSpawner};

/// Spawns tasks on the Tokio runtime that is active when the task is spawned.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSpawner;

impl TaskSpawner for TokioSpawner {
    fn spawn(&self, task: Pin<Box<dyn Future<Output = ()> + Send>>) -> Result<(), SpawnError> {
        let handle = Handle::try_current().map_err(|_| SpawnError::NoRuntime)?;
        // Dropping a Tokio JoinHandle detaches the task, so this is
        // fire-and-forget.
        let join: tokio::task::JoinHandle<()> = handle.spawn(task);
        drop(join);
        Ok(())
    }
}

/// Runs the app on a new current-thread Tokio runtime with I/O and time
/// drivers enabled.
///
/// # Errors
///
/// Returns [`RunnerError`] when the runtime cannot be built or a schedule run
/// fails.
pub fn run(app: App) -> Result<(), RunnerError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(app.run_async())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{TokioSpawner, run};
    use crate::app::App;
    use crate::runner::AsyncExecutor;

    #[test]
    fn tokio_spawner_runs_submitted_tasks() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");
        let mut app = App::new();
        app.set_spawner(TokioSpawner);

        let executor = {
            let executor = app
                .world()
                .resource::<AsyncExecutor>()
                .expect("executor resource");
            (*executor).clone()
        };

        let flag = Arc::new(AtomicBool::new(false));
        let task_flag = flag.clone();
        let loop_flag = flag.clone();
        runtime.block_on(async move {
            executor
                .spawn(async move { task_flag.store(true, Ordering::SeqCst) })
                .expect("tokio runtime is active");
            for _ in 0..100 {
                if loop_flag.load(Ordering::SeqCst) {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("spawned task did not run");
        });
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn run_exits_when_shutdown_is_cancelled() {
        let app = App::new();
        app.shutdown_handle().cancel();
        run(app).expect("run exits on shutdown");
    }
}

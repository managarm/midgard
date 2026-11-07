//! The agent's runtime: the main thread runs all tasks (sessions, the reaper, ...) on
//! one executor.

use std::future::Future;

use async_executor::{LocalExecutor, Task};

thread_local! {
    static EXECUTOR: LocalExecutor<'static> = const { LocalExecutor::new() };
}

/// Runs the tasks until `future` completes.
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
    EXECUTOR.with(|executor| async_io::block_on(executor.run(future)))
}

/// Starts a task (from the thread that runs [`block_on`]); dropping it cancels it.
pub fn spawn<T: 'static>(future: impl Future<Output = T> + 'static) -> Task<T> {
    EXECUTOR.with(|executor| executor.spawn(future))
}

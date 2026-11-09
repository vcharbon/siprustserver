//! The tokio runtime a worker binary runs on, with its thread stack size set
//! explicitly (ADR-0051 §4): a stack overflow aborts the process, so the
//! budget is stated, not inherited.

use std::future::Future;
use std::process::ExitCode;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};

/// The stack of every runtime thread, workers and blocking pool alike.
pub const THREAD_STACK_BYTES: usize = 4 * 1024 * 1024;

/// The most an exit waits for the runtime's threads. A thread held in one poll
/// never returns to the scheduler, and dropping the runtime would wait for it
/// for ever, so a wedged loop would hold the exit it asked for.
pub const EXIT_THREADS_BOUND: Duration = Duration::from_secs(1);

/// The multi-thread runtime with every driver enabled, as `#[tokio::main]`
/// builds it, and threads of [`THREAD_STACK_BYTES`].
pub fn runtime() -> std::io::Result<Runtime> {
    Builder::new_multi_thread().enable_all().thread_stack_size(THREAD_STACK_BYTES).build()
}

/// Run `serve` to its exit code on [`runtime`], then return within
/// [`EXIT_THREADS_BOUND`] whatever its threads are doing; a runtime that does
/// not build fails the process, naming `binary`.
pub fn block_on(binary: &str, serve: impl Future<Output = ExitCode>) -> ExitCode {
    block_on_within(binary, serve, EXIT_THREADS_BOUND)
}

fn block_on_within(
    binary: &str,
    serve: impl Future<Output = ExitCode>,
    threads: Duration,
) -> ExitCode {
    match runtime() {
        Ok(runtime) => {
            let code = runtime.block_on(serve);
            runtime.shutdown_timeout(threads);
            code
        }
        Err(e) => {
            eprintln!("{binary}: the tokio runtime did not build: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};

    use super::*;

    /// A task that holds its thread in one poll cannot hold the exit: the
    /// exit code comes back within the bound. Real threads and a real clock:
    /// the wedge is a thread that never returns to the runtime.
    #[test]
    fn a_thread_held_in_one_poll_does_not_hold_the_exit() {
        let release = Arc::new(AtomicBool::new(false));
        let held = release.clone();
        let (exited, exit) = mpsc::channel();
        let process = std::thread::spawn(move || {
            let serve = async move {
                let (started, start) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let _ = started.send(());
                    while !held.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                });
                let _ = start.await;
                ExitCode::FAILURE
            };
            let code = block_on_within("test", serve, Duration::from_millis(20));
            let _ = exited.send(code);
        });
        let code = exit.recv_timeout(Duration::from_millis(500));
        release.store(true, Ordering::Relaxed);
        assert_eq!(code, Ok(ExitCode::FAILURE), "the exit came back with the thread still held");
        process.join().expect("the process thread ends");
    }
}

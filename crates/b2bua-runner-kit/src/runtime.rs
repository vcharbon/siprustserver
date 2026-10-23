//! The tokio runtime a worker binary runs on, with its thread stack size set
//! explicitly (ADR-0051 §4): a stack overflow aborts the process, so the
//! budget is stated, not inherited.

use std::future::Future;
use std::process::ExitCode;

use tokio::runtime::{Builder, Runtime};

/// The stack of every runtime thread, workers and blocking pool alike.
pub const THREAD_STACK_BYTES: usize = 4 * 1024 * 1024;

/// The multi-thread runtime with every driver enabled, as `#[tokio::main]`
/// builds it, and threads of [`THREAD_STACK_BYTES`].
pub fn runtime() -> std::io::Result<Runtime> {
    Builder::new_multi_thread().enable_all().thread_stack_size(THREAD_STACK_BYTES).build()
}

/// Run `serve` to its exit code on [`runtime`]; a runtime that does not
/// build fails the process, naming `binary`.
pub fn block_on(binary: &str, serve: impl Future<Output = ExitCode>) -> ExitCode {
    match runtime() {
        Ok(runtime) => runtime.block_on(serve),
        Err(e) => {
            eprintln!("{binary}: the tokio runtime did not build: {e}");
            ExitCode::FAILURE
        }
    }
}

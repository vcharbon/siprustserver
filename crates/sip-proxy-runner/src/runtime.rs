//! The tokio runtime the proxy runs on, with its thread stack size set
//! explicitly (ADR-0051 §4): a stack overflow aborts the process, so the
//! budget is stated, not inherited. Its threads register with the process's
//! poll watchdog on their first poll (ADR-0057 §6), which the proxy starts
//! once its configuration is read.

use std::future::Future;
use std::process::ExitCode;

use tokio::runtime::{Builder, Runtime};

/// The stack of every runtime thread, recv shards and blocking pool alike.
pub const THREAD_STACK_BYTES: usize = 4 * 1024 * 1024;

/// The multi-thread runtime with every driver enabled, as `#[tokio::main]`
/// builds it, threads of [`THREAD_STACK_BYTES`], and the poll hooks of the
/// process's watchdog.
pub fn runtime() -> std::io::Result<Runtime> {
    let mut builder = Builder::new_multi_thread();
    builder.enable_all().thread_stack_size(THREAD_STACK_BYTES);
    poll_watchdog::Watchdog::process().install(&mut builder);
    builder.build()
}

/// Run `serve` to its end on [`runtime`]; a runtime that does not build
/// fails the process.
pub fn block_on(serve: impl Future<Output = ()>) -> ExitCode {
    match runtime() {
        Ok(runtime) => {
            runtime.block_on(serve);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sip-proxy-runner: the tokio runtime did not build: {e}");
            ExitCode::FAILURE
        }
    }
}

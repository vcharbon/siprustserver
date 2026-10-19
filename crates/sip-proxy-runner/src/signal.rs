//! The SIGTERM (pod termination) that starts the proxy's drain.

use std::future::Future;

/// The SIGTERM that starts the drain. A handler that cannot be installed
/// refuses the boot: without it the proxy is killed undrained.
#[cfg(unix)]
pub fn sigterm() -> Result<impl Future<Output = ()>, String> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())
        .map_err(|e| format!("cannot install the SIGTERM handler: {e}"))?;
    Ok(async move {
        term.recv().await;
    })
}

/// Never resolves: only Ctrl-C drives shutdown off unix.
#[cfg(not(unix))]
pub fn sigterm() -> Result<impl Future<Output = ()>, String> {
    Ok(std::future::pending::<()>())
}

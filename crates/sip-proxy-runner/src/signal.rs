//! The SIGTERM (pod termination) that starts the proxy's drain.

/// The SIGTERM that starts the drain. A handler that cannot be installed is
/// fatal: without it the proxy is killed undrained.
#[cfg(unix)]
pub fn sigterm() -> impl std::future::Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).unwrap_or_else(|e| {
        panic!("sip-proxy-runner FATAL: cannot install the SIGTERM handler: {e}")
    });
    async move {
        term.recv().await;
    }
}

/// Never resolves: only Ctrl-C drives shutdown off unix.
#[cfg(not(unix))]
pub fn sigterm() -> impl std::future::Future<Output = ()> {
    std::future::pending::<()>()
}

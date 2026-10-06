//! The remote address of the connection a served request arrived on, visible
//! to the service for the duration of its answer.
//!
//! Set by the real transport around each request it hands a service; the
//! simulated fabric carries no source address, so there it is unset.

use std::future::Future;
use std::net::SocketAddr;

tokio::task_local! {
    static PEER: SocketAddr;
}

/// Run `answer` with `peer` as the current request's remote address.
#[cfg_attr(not(feature = "real"), allow(dead_code))]
pub(crate) async fn scope<F: Future>(peer: SocketAddr, answer: F) -> F::Output {
    PEER.scope(peer, answer).await
}

/// The remote address of the request being answered, when the transport knows it.
pub(crate) fn current() -> Option<SocketAddr> {
    PEER.try_with(|peer| *peer).ok()
}

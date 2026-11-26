//! A [`ResolvedTarget`] as the client's name resolver: a backend client
//! looks its one name up through its target, so the lookup is bounded and
//! single-flight, every address is tried, and forgetting the target makes the
//! next connection look the name up again. A client with no target dials
//! socket addresses only, which the connector never asks a resolver about.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper_util::client::legacy::connect::dns::Name;

use crate::resolved_target::ResolvedTarget;

/// The resolver of a client: its target's lookup, or none.
#[derive(Clone)]
pub(super) struct TargetResolve(pub(super) Option<Arc<ResolvedTarget>>);

/// The name did not resolve.
#[derive(Debug, thiserror::Error)]
#[error("dns error: {0} did not resolve")]
pub(super) struct NotResolved(String);

impl tower_service::Service<Name> for TargetResolve {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = NotResolved;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, NotResolved>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), NotResolved>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let target = self.0.clone();
        Box::pin(async move {
            let addrs = match target {
                Some(target) => target.resolve_all().await,
                None => None,
            };
            match addrs {
                Some(addrs) => Ok(addrs.iter().copied().collect::<Vec<_>>().into_iter()),
                None => Err(NotResolved(name.as_str().to_string())),
            }
        })
    }
}

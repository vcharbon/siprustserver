//! A [`ResolvedTarget`] as `reqwest`'s name resolver: a backend client looks
//! its one name up through its target, so the lookup is bounded and
//! single-flight, every address is tried, and forgetting the target makes the
//! next connection look the name up again.

use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::resolved_target::ResolvedTarget;

/// `reqwest`'s resolver over one target. The client it serves only ever
/// names that target, so the name asked is the target's own.
pub(super) struct TargetResolve(pub(super) Arc<ResolvedTarget>);

/// The target's name did not resolve.
#[derive(Debug, thiserror::Error)]
#[error("dns error: {0} did not resolve")]
struct NotResolved(String);

impl Resolve for TargetResolve {
    fn resolve(&self, name: Name) -> Resolving {
        let target = self.0.clone();
        Box::pin(async move {
            match target.resolve_all().await {
                Some(addrs) => Ok(Box::new((0..addrs.len()).map(move |i| addrs[i])) as Addrs),
                None => Err(Box::new(NotResolved(name.as_str().to_string())) as _),
            }
        })
    }
}

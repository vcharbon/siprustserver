//! The lane's injector (`PCAP2TEST_PIVOT_V3.md` §6.6): what performs an
//! `inject` node's action. The interpreter never executes an action; it hands
//! the node to the injector once the node's `after` and `delay` hold, keeps
//! the call running while it waits, and marks the node done on the answer, so
//! what is ordered `after` it proceeds. A refusal or no answer within
//! [`Injector::within`] fails the run by name and ends its script.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// One `inject` node, as the run hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectRequest {
    /// The node's id.
    pub node: String,
    /// The action token, from the deployment's injector registry.
    pub action: String,
    /// What the action applies to, in the injector's vocabulary, where the
    /// node states it.
    pub target: Option<String>,
}

/// The injector's answer: `Ok` once the action was performed, `Err` with the
/// injector's reason when it refused or failed it.
pub type Injection = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// The lane's injector. One call per `inject` node per run.
pub trait Injector: Send + Sync {
    /// Starts performing `request`; the future resolves with the answer.
    fn inject(&self, request: InjectRequest) -> Injection;

    /// How long the run waits for an answer before it fails the node, on the
    /// run's tokio clock. On a paused runtime that bound is virtual: it
    /// elapses as soon as nothing else is due, so an injector whose answer
    /// waits on real I/O serves a real-clock run only.
    fn within(&self) -> Duration;
}

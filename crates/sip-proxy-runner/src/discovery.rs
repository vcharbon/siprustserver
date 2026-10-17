//! The worker pool the proxy routes to — a fixed `PROXY_WORKERS` list or the
//! EndpointSlices of the worker Service (ADR-0012 D4) — with the health-write
//! seam the OPTIONS probe drives.

use std::future::Future;
use std::sync::Arc;

use sip_clock::Clock;
use sip_proxy::registry::composed::ComposedWorkerRegistry;
use sip_proxy::registry::control::WorkerRegistryControl;
use sip_proxy::registry::static_reg::{parse_worker_list, StaticWorkerRegistry};
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use topology::{K8sMembership, Membership};

/// A worker pool and its health-write seam.
pub struct WorkerPool {
    pub registry: Arc<dyn WorkerRegistry>,
    pub control: Arc<dyn WorkerRegistryControl>,
    /// The EndpointSlice watch, `None` for a fixed list.
    pub discovery: Option<Arc<K8sMembership>>,
}

/// The fixed pool `workers` lists (`id@host:port,..`), each worker `Unknown`
/// until it answers a probe (ADR-0050 §4).
pub fn static_pool(workers: &str, clock: Clock) -> Result<WorkerPool, String> {
    let entries = parse_worker_list("PROXY_WORKERS", workers)
        .map_err(|e| format!("bad PROXY_WORKERS {workers:?}: {e}"))?
        .into_iter()
        .map(|e| WorkerEntry { health: WorkerHealth::Unknown, ..e })
        .collect();
    let reg = StaticWorkerRegistry::from_entries_with_clock(entries, clock);
    tracing::info!(source = "PROXY_WORKERS", %workers, "worker pool");
    let control = reg.control();
    Ok(WorkerPool { registry: Arc::new(reg), control, discovery: None })
}

/// The pool discovered from the EndpointSlices of `service` in `namespace`,
/// through the in-cluster (or kubeconfig) kube client.
pub fn discovered_pool(namespace: String, service: String, port: u16, clock: Clock) -> WorkerPool {
    discovered_pool_with(
        || async { kube::Client::try_default().await.map_err(|e| e.to_string()) },
        namespace,
        service,
        port,
        clock,
    )
}

/// [`discovered_pool`] over the kube client `connect` builds. The pool starts
/// empty; a client that fails to build is built again in the background
/// (1 s doubling to 30 s, for the life of the process), and workers join as
/// the watch lists them. A broken watch keeps the last listed pool.
pub fn discovered_pool_with<F, Fut>(
    connect: F,
    namespace: String,
    service: String,
    port: u16,
    clock: Clock,
) -> WorkerPool
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Result<kube::Client, String>> + Send,
{
    // Installing the provider a second time returns Err; ignored.
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing::info!(source = "k8s-endpointslice", %service, %namespace, port, "worker pool");
    let watch = Arc::new(K8sMembership::spawn_connecting(connect, namespace, service));
    let membership: Arc<dyn Membership> = watch.clone();
    let reg = ComposedWorkerRegistry::spawn(membership, port, clock);
    let control = reg.control();
    WorkerPool { registry: Arc::new(reg), control, discovery: Some(watch) }
}

#[cfg(test)]
mod tests;

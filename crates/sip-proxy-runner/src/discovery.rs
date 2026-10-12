//! The worker pool the proxy routes to — a fixed `PROXY_WORKERS` list or the
//! EndpointSlices of the worker Service (ADR-0012 D4) — with the health-write
//! seam the OPTIONS probe drives.

use std::future::Future;
use std::sync::Arc;

use sip_clock::Clock;
use sip_proxy::registry::composed::ComposedWorkerRegistry;
use sip_proxy::registry::control::WorkerRegistryControl;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use topology::{K8sMembership, Membership};

/// A worker pool and its health-write seam.
pub struct WorkerPool {
    pub registry: Arc<dyn WorkerRegistry>,
    pub control: Arc<dyn WorkerRegistryControl>,
    /// The EndpointSlice watch, `None` for a fixed list.
    pub discovery: Option<Arc<K8sMembership>>,
}

/// The fixed pool `workers` lists (`id@host:port,..`).
pub fn static_pool(workers: &str, clock: Clock) -> Result<WorkerPool, String> {
    let reg = StaticWorkerRegistry::from_string_with_clock(workers, "PROXY_WORKERS", clock)
        .map_err(|e| format!("bad PROXY_WORKERS {workers:?}: {e}"))?;
    tracing::info!(source = "PROXY_WORKERS", %workers, "worker pool");
    let control = reg.control();
    Ok(WorkerPool { registry: Arc::new(reg), control, discovery: None })
}

/// The pool discovered from the EndpointSlices of `service` in `namespace`,
/// through the in-cluster (or kubeconfig) kube client.
pub async fn discovered_pool(
    namespace: String,
    service: String,
    port: u16,
    clock: Clock,
) -> WorkerPool {
    discovered_pool_with(
        || async { kube::Client::try_default().await.map_err(|e| e.to_string()) },
        namespace,
        service,
        port,
        clock,
    )
    .await
}

/// [`discovered_pool`] over the kube client `connect` builds.
pub async fn discovered_pool_with<F, Fut>(
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
    match connect().await {
        Ok(client) => {
            tracing::info!(source = "k8s-endpointslice", %service, %namespace, port, "worker pool");
            let watch = Arc::new(K8sMembership::spawn(client, namespace, service));
            let membership: Arc<dyn Membership> = watch.clone();
            let reg = ComposedWorkerRegistry::spawn(membership, port, clock);
            let control = reg.control();
            WorkerPool { registry: Arc::new(reg), control, discovery: Some(watch) }
        }
        Err(e) => {
            let fallback = "w0@127.0.0.1:5060";
            tracing::warn!(
                error = %e,
                %fallback,
                "no kube client and no PROXY_WORKERS — falling back to a single local worker"
            );
            let reg =
                StaticWorkerRegistry::from_string(fallback, "fallback").expect("fallback pool");
            let control = reg.control();
            WorkerPool { registry: Arc::new(reg), control, discovery: None }
        }
    }
}

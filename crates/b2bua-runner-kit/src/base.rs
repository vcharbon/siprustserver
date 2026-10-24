//! [`RunnerBase`]: what a bound runner shares between its assembly steps.

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::admission::Refusals;
use b2bua::config::B2buaConfig;
use b2bua::metrics::{B2buaMetrics, UdpTransportMetrics};
use sip_clock::Clock;
use sip_net::UdpEndpoint;

use crate::RunnerEnv;

/// Everything a bound runner shares between its assembly steps: the endpoint,
/// the validated config, and the process-wide metrics/clock. All fields pub so
/// a runner can reach any piece (e.g. `base.metrics` for a custom CDR sink,
/// `base.clock` for a replication store).
pub struct RunnerBase {
    /// Log-line prefix (the binary's name).
    pub name: String,
    /// The parsed env this base was built from (post any field overrides).
    pub env: RunnerEnv,
    /// The shared bound UDP endpoint (core recv loop + live metrics getters).
    pub endpoint: Arc<dyn UdpEndpoint>,
    /// The actually-bound local address.
    pub local: SocketAddr,
    /// The validated worker config (advertise coercion applied).
    pub config: B2buaConfig,
    /// This worker's identity, one boot of its ordinal (ADR-0055 §6);
    /// [`deps`](Self::deps) hands it to the core.
    pub identity: String,
    /// The `UdpTransport` metrics facade over [`Self::endpoint`].
    pub udp_metrics: UdpTransportMetrics,
    /// The worker's identifier generator, keyed from the OS RNG at bind;
    /// [`deps`](Self::deps) hands it to the core.
    pub id_gen: Arc<sip_txn::IdGen>,
    /// The worker's refusals of new INVITEs, held by the ingress brake on
    /// [`Self::endpoint`]; [`deps`](Self::deps) hands the core the same
    /// instance, so a runner swapping one swaps both.
    pub refusals: Refusals,
    /// The process-wide metrics registry the core exports at `/metrics`.
    pub metrics: B2buaMetrics,
    /// System wall clock (transaction/dialog timers fire for real).
    pub clock: Clock,
    /// Resolved probe/metrics HTTP listen address.
    pub metrics_sa: SocketAddr,
}

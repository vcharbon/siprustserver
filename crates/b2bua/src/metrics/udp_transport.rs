//! `UdpTransportMetrics` — the `UdpTransport` facade's Prometheus-visible
//! shape: live getters over the bound endpoint and the ingress brake's
//! counters.

use std::sync::Arc;

use sip_net::UdpEndpointCounters;

use super::catalogue;
use crate::ingress_brake::IngressBrakeCounters;

/// A live-read source for an endpoint gauge (`queueDepth`). `Arc<dyn Fn>` so
/// the surface is decoupled from the concrete `UdpEndpoint` type (which is held
/// as a `Box<dyn UdpEndpoint>` by the runner and is not `Clone`); the closure
/// captures a clone of the shared counter/queue handle and reads it on demand.
/// `Send + Sync` because the `/metrics` scrape may run on any task.
pub type LiveGauge = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A live-read source for the endpoint's counters (`endpoint.counters()`), read
/// once per scrape.
pub type LiveCounters = Arc<dyn Fn() -> UdpEndpointCounters + Send + Sync>;

/// The `UdpTransport` facade's Prometheus-visible shape: a bag of **live
/// getters**. Both the scrape endpoint and test reads want the *instantaneous*
/// value, never a cached snapshot, so every facet reads through on each access:
///
///   - `queue_depth` → an injected [`LiveGauge`] backed by the underlying
///     [`UdpEndpoint`](sip_net::UdpEndpoint) (`endpoint.queueDepth()`).
///   - `queue_max` → the bind's configured bound (a constant, copied once).
///   - the endpoint's lifetime counters → an injected [`LiveCounters`] over
///     `endpoint.counters()`: tail drops, sends refused by a full send buffer,
///     datagrams the kernel dropped before the queue saw them, datagrams whose
///     pre-ingress hook panicked (each accepted, each a defect), and failed
///     receives.
///   - `ingress_brake_emergency_bypassed` → the shared [`IngressBrakeCounters`] the
///     `preIngress` hook mutates; its refusals are new-call counts
///     ([`crate::new_calls`]).
///
/// Clone-cheap (all fields are `Arc`/`Copy`): the runner keeps one to render
/// `/metrics` and may hand clones to other readers. The runner builds it from
/// the bound endpoint + brake counters and concatenates
/// [`Self::prometheus_text`] into the `/metrics` body.
#[derive(Clone)]
pub struct UdpTransportMetrics {
    queue_depth: LiveGauge,
    queue_max: usize,
    counters: LiveCounters,
    brake: IngressBrakeCounters,
}

impl std::fmt::Debug for UdpTransportMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Closures aren't Debug — render the live values instead.
        f.debug_struct("UdpTransportMetrics")
            .field("queue_depth", &self.queue_depth())
            .field("queue_max", &self.queue_max)
            .field("counters", &self.counters())
            .field("ingress_brake_emergency_bypassed", &self.ingress_brake_emergency_bypassed())
            .finish()
    }
}

impl UdpTransportMetrics {
    /// Build the shape from its live sources.
    ///
    ///   - `queue_max`: the bind's configured queue bound (`config.udpQueueMax`).
    ///   - `brake`: the [`IngressBrakeCounters`] the `preIngress` hook holds (so
    ///     `ingress_brake_emergency_bypassed` reads live).
    ///   - `queue_depth` / `counters`: live getters over the bound endpoint
    ///     (typically `move || endpoint.queue_depth()` and
    ///     `move || endpoint.counters()` with a shared handle).
    pub fn new(
        queue_max: usize,
        brake: IngressBrakeCounters,
        queue_depth: LiveGauge,
        counters: LiveCounters,
    ) -> Self {
        Self { queue_depth, queue_max, counters, brake }
    }

    /// Live inbound-queue depth (`endpoint.queueDepth()`).
    pub fn queue_depth(&self) -> u64 {
        (self.queue_depth)()
    }
    /// The configured inbound-queue bound (`config.udpQueueMax`).
    pub fn queue_max(&self) -> usize {
        self.queue_max
    }
    /// The endpoint's lifetime counters, read now.
    pub fn counters(&self) -> UdpEndpointCounters {
        (self.counters)()
    }
    /// Datagrams the full inbound queue tail-dropped (`dropsTailDrop`, live ←
    /// `endpoint.counters.tailDropped`).
    pub fn drops_tail_drop(&self) -> u64 {
        self.counters().tail_dropped
    }
    /// New emergency INVITEs that bypassed the ingress brake above the threshold.
    pub fn ingress_brake_emergency_bypassed(&self) -> u64 {
        self.brake.emergency_bypassed()
    }
    /// The brake's counters, for the new-call outcome count
    /// ([`crate::new_calls::NewCallCounts::read`]).
    pub fn brake(&self) -> &IngressBrakeCounters {
        &self.brake
    }

    /// Render the shape as Prometheus text exposition for the `/metrics` body.
    /// All series use the `b2bua_udp_*` namespace.
    ///
    /// Counters (monotonic): `ingress_brake_emergency_bypassed`, `tail_dropped`,
    /// `send_would_block`, `kernel_rx_dropped`, `pre_ingress_panics`,
    /// `recv_errors`. Gauges
    /// (instantaneous): `queue_depth`, `queue_max`. The brake's refusals are
    /// new-call counts (`b2bua_new_calls_total`), rendered with the other
    /// admission sources.
    pub fn prometheus_text(&self) -> String {
        let mut s = String::with_capacity(1536);

        // ── Ingress brake: emergency INVITEs let through above the threshold ──
        catalogue::UDP_INGRESS_BRAKE_EMERGENCY_BYPASSED
            .render_value(&mut s, self.ingress_brake_emergency_bypassed());

        // ── Inbound queue state (live getters over the endpoint). A
        //    tail-dropping queue otherwise shows 100% accepted, hiding a burst
        //    collapse. ──
        catalogue::udp::QUEUE_DEPTH.render_value(&mut s, self.queue_depth());
        catalogue::udp::QUEUE_MAX.render_value(&mut s, self.queue_max() as u64);
        let counters = self.counters();
        catalogue::udp::TAIL_DROPPED.render_value(&mut s, counters.tail_dropped);
        catalogue::udp::SEND_WOULD_BLOCK.render_value(&mut s, counters.send_would_block);
        catalogue::udp::KERNEL_RX_DROPPED.render_value(&mut s, counters.kernel_rx_dropped);
        catalogue::udp::PRE_INGRESS_PANICS.render_value(&mut s, counters.pre_ingress_panics);
        catalogue::udp::RECV_ERRORS.render_value(&mut s, counters.recv_errors);
        s
    }
}

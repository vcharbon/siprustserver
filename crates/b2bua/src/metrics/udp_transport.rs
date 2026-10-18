//! `UdpTransportMetrics` — the `UdpTransport` facade's Prometheus-visible
//! shape: live getters over the bound endpoint and the ingress brake's
//! counters.

use std::sync::Arc;

use super::catalogue;
use crate::ingress_brake::IngressBrakeCounters;

/// A live-read source for an endpoint gauge (`queueDepth`, `dropsTailDrop`,
/// `sendWouldBlock`, `kernelRxDropped`). `Arc<dyn Fn>` so the surface is decoupled from the
/// concrete `UdpEndpoint` type (which is held as a `Box<dyn UdpEndpoint>` by the
/// runner and is not `Clone`); the closure captures a clone of the shared
/// counter/queue handle and reads it on demand. `Send + Sync` because the
/// `/metrics` scrape may run on any task.
pub type LiveGauge = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The `UdpTransport` facade's Prometheus-visible shape: a bag of **live
/// getters**. Both the scrape endpoint and test reads want the *instantaneous*
/// value, never a cached snapshot, so every facet reads through on each access:
///
///   - `queue_depth` / `drops_tail_drop` → injected [`LiveGauge`]s backed by the
///     underlying [`UdpEndpoint`](sip_net::UdpEndpoint) (`endpoint.queueDepth()` /
///     `endpoint.counters().tail_dropped`).
///   - `queue_max` → the bind's configured bound (a constant, copied once).
///   - `kernel_rx_dropped` → an injected [`LiveGauge`] over
///     `endpoint.counters().kernel_rx_dropped`: datagrams the kernel dropped
///     before the queue saw them, which no other facet counts.
///   - `pre_ingress_panics` → an injected [`LiveGauge`] over
///     `endpoint.counters().pre_ingress_panics`: datagrams whose pre-ingress
///     hook panicked (each accepted, each a defect).
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
    drops_tail_drop: LiveGauge,
    /// Outbound datagrams the socket refused because its send buffer was
    /// full (ADR-0033): a live getter over the bound endpoint.
    send_would_block: LiveGauge,
    /// Datagrams the kernel dropped on the socket before the receive pump read
    /// them (a full `SO_RCVBUF`): a live getter over the bound endpoint.
    kernel_rx_dropped: LiveGauge,
    /// Datagrams whose pre-ingress hook panicked: a live getter over the bound
    /// endpoint.
    pre_ingress_panics: LiveGauge,
    brake: IngressBrakeCounters,
}

impl std::fmt::Debug for UdpTransportMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Closures aren't Debug — render the live values instead.
        f.debug_struct("UdpTransportMetrics")
            .field("queue_depth", &self.queue_depth())
            .field("queue_max", &self.queue_max)
            .field("drops_tail_drop", &self.drops_tail_drop())
            .field("send_would_block", &self.send_would_block())
            .field("kernel_rx_dropped", &self.kernel_rx_dropped())
            .field("pre_ingress_panics", &self.pre_ingress_panics())
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
    ///   - `queue_depth` / `drops_tail_drop`: live getters over the bound
    ///     endpoint (typically `move || endpoint.queue_depth()` and
    ///     `move || endpoint.counters().tail_dropped` with a shared handle).
    ///   - `send_would_block` / `kernel_rx_dropped` / `pre_ingress_panics`:
    ///     live getters over the same endpoint's `counters()`.
    pub fn new(
        queue_max: usize,
        brake: IngressBrakeCounters,
        queue_depth: LiveGauge,
        drops_tail_drop: LiveGauge,
        send_would_block: LiveGauge,
        kernel_rx_dropped: LiveGauge,
        pre_ingress_panics: LiveGauge,
    ) -> Self {
        Self {
            queue_depth,
            queue_max,
            drops_tail_drop,
            send_would_block,
            kernel_rx_dropped,
            pre_ingress_panics,
            brake,
        }
    }

    /// Live inbound-queue depth (`endpoint.queueDepth()`).
    pub fn queue_depth(&self) -> u64 {
        (self.queue_depth)()
    }
    /// The configured inbound-queue bound (`config.udpQueueMax`).
    pub fn queue_max(&self) -> usize {
        self.queue_max
    }
    /// Datagrams the full inbound queue tail-dropped (`dropsTailDrop`, live ←
    /// `endpoint.counters.tailDropped`).
    pub fn drops_tail_drop(&self) -> u64 {
        (self.drops_tail_drop)()
    }
    /// Outbound datagrams refused by a full send buffer (never a suspended
    /// send — ADR-0033).
    pub fn send_would_block(&self) -> u64 {
        (self.send_would_block)()
    }
    /// Datagrams the kernel dropped before the receive pump read them.
    pub fn kernel_rx_dropped(&self) -> u64 {
        (self.kernel_rx_dropped)()
    }
    /// Datagrams whose pre-ingress hook panicked.
    pub fn pre_ingress_panics(&self) -> u64 {
        (self.pre_ingress_panics)()
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
    /// `send_would_block`, `kernel_rx_dropped`, `pre_ingress_panics`. Gauges
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
        catalogue::udp::TAIL_DROPPED.render_value(&mut s, self.drops_tail_drop());
        catalogue::udp::SEND_WOULD_BLOCK.render_value(&mut s, self.send_would_block());
        catalogue::udp::KERNEL_RX_DROPPED.render_value(&mut s, self.kernel_rx_dropped());
        catalogue::udp::PRE_INGRESS_PANICS.render_value(&mut s, self.pre_ingress_panics());
        s
    }
}

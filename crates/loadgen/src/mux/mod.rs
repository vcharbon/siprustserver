//! Multiplexed SIP transport — a SIPp-style replacement for one-socket-per-call.
//!
//! # Shape
//!
//! A [`MuxCore`] owns a small, fixed set of **named endpoints**, each = exactly
//! one datagram endpoint on the underlying fabric with a dispatcher recv-loop.
//! The fabric is a [`SignalingNetwork`] seam: real UDP by default
//! ([`MuxCore::bind`]), or any other impl — notably the simulated network under
//! the paused-clock test lane — via [`MuxCore::bind_on`]. Each endpoint
//! multiplexes *many* concurrent dialogs, routing inbound datagrams to the
//! right call's inbox by dialog identity. So O(endpoints) sockets regardless of
//! CPS — no fd / ephemeral-port exhaustion, and the SUT routes the callee leg
//! to a **static** address (its own config), never a dynamic per-call one.
//!
//! # Module map
//!
//! - [`correlation`] — how the per-call token travels through the SUT
//!   (pluggable per run: relayed header, or To-user).
//! - [`demux`] — the inbound path. Precedence per datagram: (1) **known
//!   Call-ID** (our UAC dialog, or a UAS dialog after its first request) — this
//!   tier demuxes EVERY in-dialog datagram with no token and no R-URI
//!   cooperation; (2) **correlation token** — an initial INVITE spawning a new
//!   leg (the SUT carries the token unchanged onto every originated leg, so
//!   callee and transfer-target legs share one token); (3) orphan
//!   (count + bounded-sample + drop — never queued, never silent).
//! - [`endpoint`] — the per-call [`SignalingNetwork`] view ([`MuxNetwork`]) and
//!   each leg's endpoint. Every per-call endpoint deregisters its keys on
//!   `Drop`; a reaper sweeps pending-UAS entries whose leg never arrived.
//! - [`loss`] — per-call loss injection: the network layer's
//!   [`sip_net::RandomLoss`] + the deterministic, test-owned [`TargetedDrop`].
//! - [`retransmit`] — opt-in per-call SIP transaction engine (Timer A/E/G)
//!   recovering modeled loss.
//! - [`stats`] — process-wide counters + Prometheus rendering.
//!
//! Recording sits *after* the UDP/demux layer, per sampled call: the existing
//! `AgentBinder::with_network(mux, …, record)` wraps a per-call endpoint with
//! the recording fake layer — free when off.

mod correlation;
mod demux;
mod endpoint;
mod loss;
mod retransmit;
mod stats;

pub use correlation::Correlation;
pub use endpoint::{CallRouting, MuxNetwork};
pub use loss::{DropDir, TargetedDrop};
pub use stats::MuxStats;

use loss::DropModel;
use retransmit::CallTxns;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_net::queue::PacketQueue;
use sip_net::{BindUdpOpts, RealSignalingNetwork, SignalingNetwork, UdpEndpoint};
// Monotonic time rides `tokio::time` so the pending-slot deadlines and the reap
// sweep move with the (possibly paused) runtime clock — under `start_paused` a
// `std::time::Instant` would barely advance and the reaper would go inert.
use tokio::task::JoinHandle;
use tokio::time::Instant;

// The R-URI leg-picker lives in its shared home, `scenario-harness`: the same
// primitive backs the functional/e2e multi-callee facility
// (`scenario_harness::callee_group`) and this load mux's second demux tier.
// Re-exported so `loadgen::{LegInfo, LegPicker, prefix_leg_picker}` and
// `crate::mux::LegInfo` keep resolving — the mux consumes it, it does not own it.
pub use scenario_harness::legpick::{
    labelled_prefix_leg_picker, labelled_prefix_leg_picker_defaulting, prefix_leg_picker, LegInfo,
    LegPicker,
};

/// Whether a defined endpoint originates calls (UAC) or receives them (UAS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Caller: registers its dialog by the Call-ID sniffed from its outbound
    /// INVITE; receives responses + in-dialog by that Call-ID.
    Caller,
    /// Callee: registers a correlation token at bind; the SUT-delivered initial
    /// INVITE is matched by token, then promoted to its Call-ID.
    Callee,
}

/// A registry key owned by one endpoint (removed on its `Drop`).
#[derive(Debug, Clone)]
pub(in crate::mux) enum Key {
    CallId(String),
    /// A receiver registered under `token`, identified by its `label` (agent
    /// name) so `Drop` removes exactly this receiver from a possibly-shared slot.
    Token { token: String, label: String },
}

/// Everything the inbound route path needs to hand a datagram to one call: its
/// inbox, its loss model (applied above the retransmit engine), and its
/// optional retransmit engine (present iff `--auto-retransmit` is on for the call).
#[derive(Clone)]
pub(in crate::mux) struct Delivery {
    pub(in crate::mux) queue: Arc<PacketQueue>,
    pub(in crate::mux) drop: Arc<DropModel>,
    pub(in crate::mux) txns: Option<Arc<CallTxns>>,
}

/// One receiver bound on a socket: an inbox + its label (the agent name a
/// [`LegPicker`] returns) + the keyset its `Drop` drains + the loss/retransmit
/// state the inbound path applies once the leg arrives.
pub(in crate::mux) struct ReceiverEntry {
    pub(in crate::mux) label: String,
    pub(in crate::mux) queue: Arc<PacketQueue>,
    pub(in crate::mux) keyset: Arc<Mutex<Vec<Key>>>,
    pub(in crate::mux) drop: Arc<DropModel>,
    pub(in crate::mux) txns: Option<Arc<CallTxns>>,
}

/// All receivers for one call on one socket, sharing the call token. Usually a
/// single receiver; >1 only when a scenario binds several endpoints on the same
/// port, in which case `picker` disambiguates each arriving leg.
pub(in crate::mux) struct CallSlot {
    pub(in crate::mux) receivers: Vec<ReceiverEntry>,
    /// Scenario-owned disambiguation, used iff `receivers.len() > 1`.
    pub(in crate::mux) picker: Option<LegPicker>,
    /// Set once any leg has arrived — the reaper then leaves the slot alone
    /// (a live call may outlast the pending deadline), so only never-arrived
    /// legs are swept.
    pub(in crate::mux) arrived: bool,
    pub(in crate::mux) deadline: Instant,
}

#[derive(Default)]
pub(in crate::mux) struct SocketRegistry {
    pub(in crate::mux) by_call_id: HashMap<String, Delivery>,
    pub(in crate::mux) by_token: HashMap<String, CallSlot>,
}

/// One defined endpoint = one fabric endpoint + dispatcher + per-socket registry.
pub(in crate::mux) struct MuxSocket {
    pub(in crate::mux) addr: SocketAddr,
    pub(in crate::mux) role: Role,
    pub(in crate::mux) endpoint: Arc<dyn UdpEndpoint>,
    pub(in crate::mux) reg: Mutex<SocketRegistry>,
    pub(in crate::mux) correlation: Correlation,
    pub(in crate::mux) queue_max: usize,
    pub(in crate::mux) stats: Arc<MuxStats>,
    /// The shared loadgen clock — stamps inbound `UdpPacket::arrival_ms` (ordering
    /// hint only; `seq` is the authority) so the mux reads no raw `SystemTime`.
    pub(in crate::mux) clock: Clock,
    _dispatcher: JoinHandle<()>,
}

/// The process-wide multiplexer: the fixed set of endpoints + shared stats +
/// the pending-entry reaper.
pub struct MuxCore {
    pub(in crate::mux) endpoints: HashMap<SocketAddr, Arc<MuxSocket>>,
    stats: Arc<MuxStats>,
    pub(in crate::mux) pending_ttl: Duration,
    _reaper: JoinHandle<()>,
}

/// One endpoint to open.
pub struct EndpointSpec {
    pub addr: SocketAddr,
    pub role: Role,
}

/// Bound on each defined endpoint's socket-level inbox (the pre-demux queue the
/// fabric's recv pump feeds, drained by the dispatcher into per-call inboxes).
/// One endpoint carries EVERY concurrent call's traffic, so it is much deeper
/// than the per-call `queue_max`.
const DISPATCH_QUEUE_MAX: usize = 4096;

impl MuxCore {
    /// Open every endpoint on the REAL network + dispatcher and start the
    /// reaper — the production/bin path. `queue_max` bounds each call's inbox;
    /// `orphan_sample_cap` bounds stored orphan samples.
    pub async fn bind(
        specs: Vec<EndpointSpec>,
        correlation: Correlation,
        queue_max: usize,
        orphan_sample_cap: usize,
        pending_ttl: Duration,
        clock: Clock,
    ) -> std::io::Result<Arc<Self>> {
        Self::bind_on(
            &RealSignalingNetwork::new(),
            specs,
            correlation,
            queue_max,
            orphan_sample_cap,
            pending_ttl,
            clock,
        )
        .await
    }

    /// [`bind`](Self::bind) over an explicit fabric — the transport seam. Pass
    /// the `SimulatedSignalingNetwork` (shared with the in-process SUT's
    /// harness) to run the REAL driver + mux + demux + loss-model stack under a
    /// paused clock with no real sockets; the loss/retransmit knobs sit ABOVE
    /// this seam and behave identically on either fabric.
    pub async fn bind_on(
        fabric: &dyn SignalingNetwork,
        specs: Vec<EndpointSpec>,
        correlation: Correlation,
        queue_max: usize,
        orphan_sample_cap: usize,
        pending_ttl: Duration,
        clock: Clock,
    ) -> std::io::Result<Arc<Self>> {
        let stats = Arc::new(MuxStats::new(orphan_sample_cap));
        let mut endpoints = HashMap::new();
        for spec in specs {
            let endpoint: Arc<dyn UdpEndpoint> = Arc::from(
                fabric
                    .bind_udp(BindUdpOpts::new(spec.addr, DISPATCH_QUEUE_MAX))
                    .await
                    .map_err(|e| {
                        std::io::Error::other(format!("mux bind {}: {}", e.addr, e.message))
                    })?,
            );
            let local = endpoint.local_addr();
            let clock = clock.clone();
            let mux = Arc::new_cyclic(|weak: &std::sync::Weak<MuxSocket>| {
                let dispatcher = tokio::spawn(demux::dispatch_loop(weak.clone(), endpoint.clone()));
                MuxSocket {
                    addr: local,
                    role: spec.role,
                    endpoint: endpoint.clone(),
                    reg: Mutex::new(SocketRegistry::default()),
                    correlation: correlation.clone(),
                    queue_max,
                    stats: stats.clone(),
                    clock,
                    _dispatcher: dispatcher,
                }
            });
            endpoints.insert(local, mux);
        }
        let reaper = tokio::spawn(demux::reap_loop(
            endpoints.values().cloned().collect::<Vec<_>>(),
            stats.clone(),
            pending_ttl,
        ));
        Ok(Arc::new(Self { endpoints, stats, pending_ttl, _reaper: reaper }))
    }

    /// All bound endpoint addresses (handy for tests).
    pub fn addrs(&self) -> Vec<SocketAddr> {
        self.endpoints.keys().copied().collect()
    }

    pub fn stats(&self) -> &Arc<MuxStats> {
        &self.stats
    }

    /// Live registry size across all endpoints (leak canary — should track
    /// in-flight calls). Counts every live receiver across all call slots plus
    /// the promoted Call-IDs.
    pub fn registry_size(&self) -> usize {
        self.endpoints
            .values()
            .map(|e| {
                let g = e.reg.lock().unwrap();
                let receivers: usize = g.by_token.values().map(|s| s.receivers.len()).sum();
                g.by_call_id.len() + receivers
            })
            .sum()
    }

    /// Build a per-call [`SignalingNetwork`] view from a [`CallRouting`] (the
    /// single call token + each callee leg's label + any same-socket picker).
    /// No simulated packet loss and no auto-retransmit.
    pub fn network(self: &Arc<Self>, routing: CallRouting) -> MuxNetwork {
        self.network_tuned(routing, 0.0, false, 0, None)
    }

    /// [`network`](Self::network) with per-call robustness knobs:
    /// - `drop_rate` (0 = off): each datagram this call's endpoints send/receive is
    ///   independently dropped, so a scenario is exercised against a lossy fabric.
    /// - `retransmit`: when set, the mux runs a per-call SIP transaction engine
    ///   that retransmits lost signaling on real timers (Timer A/E for requests,
    ///   Timer G 2xx-until-ACK for answers) and absorbs the resulting duplicates,
    ///   so a rare drop is recovered instead of failing the call.
    ///
    /// `seed` seeds each endpoint's loss RNG (derive it from the per-call id seed
    /// for reproducibility).
    /// `drop_nth` optionally adds the DETERMINISTIC targeted drop
    /// ([`TargetedDrop`]) on top of the probabilistic rate.
    pub fn network_tuned(
        self: &Arc<Self>,
        routing: CallRouting,
        drop_rate: f64,
        retransmit: bool,
        seed: u64,
        drop_nth: Option<TargetedDrop>,
    ) -> MuxNetwork {
        // Per-callee-addr bind-order label queue: each `bind_udp(addr)` dispenses
        // the next declared label for that addr (so several receivers can share a
        // socket; the driver binds them in declaration order).
        let mut labels: HashMap<SocketAddr, Vec<String>> = HashMap::new();
        for (addr, label) in &routing.legs {
            labels.entry(*addr).or_default().push(label.clone());
        }
        MuxNetwork {
            core: self.clone(),
            token: routing.token,
            labels,
            pickers: routing.pickers,
            cursor: Mutex::new(HashMap::new()),
            drop_rate,
            retransmit,
            drop_seed: AtomicU64::new(seed | 1),
            drop_nth,
        }
    }
}

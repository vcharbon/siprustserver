//! Public network-layer types — port of the type surface in
//! `src/sip/SignalingNetwork.ts`, reshaped to Rust / tokio idioms.
//!
//! Reshaping notes (the "closer to Rust methods" adaptation):
//!   - `RemoteInfo { address, port }` → [`std::net::SocketAddr`] everywhere.
//!   - `bindUdp` ip/port pair → a single `addr: SocketAddr` on [`BindUdpOpts`].
//!   - `Buffer` → `Vec<u8>` (owned) / `&[u8]` (borrowed on `send`).
//!   - `PreIngressHook` is an `Arc<dyn Fn ...>` so it is cheap to clone into
//!     the simulated fabric's routing table and into recorded summaries.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

/// Theoretical max single-UDP-datagram payload (65535 − 20 IP − 8 UDP). The
/// paranoid decorator rejects sends above this; SIP fragments far below it.
pub const MAX_UDP_PAYLOAD: usize = 65507;

/// One received datagram handed up from an endpoint's inbound queue. The TS
/// `UdpPacket` carried an optional pre-parsed `SipMessage`; here parsing is
/// the consumer's job (and the recording projector's), so the packet is raw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpPacket {
    pub raw: Vec<u8>,
    pub src: SocketAddr,
    /// Capture timestamp (ms) — wall clock on the real impl, virtual on the
    /// simulated fabric. Report/ordering only.
    pub arrival_ms: u64,
}

/// How an inbound datagram fared at the receiving endpoint's inbox, recorded on
/// `SignalingNetworkEvent::RecvItem` at DELIVERY time (newkahneed-036 ask A) so
/// the trace reflects the true wire even when the scenario body never reads the
/// packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvDisposition {
    /// Enqueued into the endpoint's inbox (the normal case).
    Delivered,
    /// Arrived but the bounded inbox was full — the app never saw it.
    InboxOverflow,
    /// Arrived after the endpoint closed its inbox — the app never saw it.
    InboxClosed,
    /// Arrived but the simulated packet-loss model discarded it (loadgen
    /// `--drop-rate`): modeled network loss.
    LossModel,
    /// Arrived but the per-call retransmit engine absorbed it as a duplicate
    /// (loadgen `--auto-retransmit`): infra dedup the app must not see.
    AbsorbedRetransmit,
}

impl RecvDisposition {
    /// Whether the RFC audit judges the receiving UA against this arrival.
    /// `Delivered`/`InboxOverflow` are true arrivals at a live endpoint;
    /// `LossModel`/`AbsorbedRetransmit` are deliberately modeled as "the UA
    /// never saw it" (auditing on them would judge behaviour the loss model
    /// forbade), and `InboxClosed` arrivals postdate the endpoint (ladder-only,
    /// like an orphan).
    pub fn audit_visible(self) -> bool {
        matches!(self, RecvDisposition::Delivered | RecvDisposition::InboxOverflow)
    }
}

/// Delivery-time tap installed on an endpoint's inbox by the recording
/// decorator (sampled/recording calls only — the non-recording path never
/// installs one). Invoked with each inbound datagram + its disposition at the
/// moment the inbox accepts or rejects it, so arrival is observed independently
/// of whether the body ever calls `recv`.
pub type RecvTap = Arc<dyn Fn(&UdpPacket, RecvDisposition) + Send + Sync>;

/// Per-endpoint counters. Snapshot of the live atomics behind an endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UdpEndpointCounters {
    pub enqueued: u64,
    pub tail_dropped: u64,
    pub pre_ingress_dropped: u64,
    pub pre_ingress_replies: u64,
}

/// SIP role(s) a bind serves (port of `UaRole`). The audit framework's
/// per-rule dispatch intersects a rule's `subject` with a bind's declared
/// roles; a `proxy`-only rule does not run against a pure-UA bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UaRole {
    Uac,
    Uas,
    Proxy,
}

/// The set treated as "every role" — a bind that declares no roles, and rules
/// that apply everywhere, use this.
pub fn all_ua_roles() -> HashSet<UaRole> {
    HashSet::from([UaRole::Uac, UaRole::Uas, UaRole::Proxy])
}

/// What a pre-ingress hook decides for an arriving datagram, at arrival time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreIngressAction {
    /// Enqueue normally.
    Accept,
    /// Silently drop (counted as `pre_ingress_dropped`).
    Drop,
    /// Don't enqueue; send these bytes back to the source (counted as
    /// `pre_ingress_replies`). The Tier-1 overload brake's stateless-503 path.
    Reply(Vec<u8>),
}

/// Arrival-time filter installed at `bind_udp`. Receives the raw bytes, the
/// source address, and the current queue depth. `Arc<dyn Fn>` so it clones
/// into the simulated routing table and recorded summaries.
pub type PreIngressHook =
    Arc<dyn Fn(&[u8], SocketAddr, usize) -> PreIngressAction + Send + Sync>;

/// Options for `bind_udp` (port of `BindUdpOpts`).
#[derive(Clone)]
pub struct BindUdpOpts {
    pub addr: SocketAddr,
    pub queue_max: usize,
    pub pre_ingress: Option<PreIngressHook>,
    /// `SO_REUSEPORT`. Honored by the real impl (socket2-built socket — see
    /// real.rs); ignored by the simulated fabric (one endpoint per addr).
    pub reuse_port: bool,
    /// SIP role(s) this bind serves. `None` → [`all_ua_roles`].
    pub roles: Option<HashSet<UaRole>>,
}

impl BindUdpOpts {
    /// Minimal opts: an address and a bounded inbound queue, no pre-ingress
    /// hook, default roles.
    pub fn new(addr: SocketAddr, queue_max: usize) -> Self {
        Self {
            addr,
            queue_max,
            pre_ingress: None,
            reuse_port: false,
            roles: None,
        }
    }

    pub fn with_pre_ingress(mut self, hook: PreIngressHook) -> Self {
        self.pre_ingress = Some(hook);
        self
    }

    /// Request `SO_REUSEPORT` (recv-shard binds — every shard on the port must
    /// set it, including the first).
    pub fn with_reuse_port(mut self, on: bool) -> Self {
        self.reuse_port = on;
        self
    }

    pub fn with_roles(mut self, roles: HashSet<UaRole>) -> Self {
        self.roles = Some(roles);
        self
    }

    /// The declared roles, defaulting to [`all_ua_roles`].
    pub fn effective_roles(&self) -> HashSet<UaRole> {
        self.roles.clone().unwrap_or_else(all_ua_roles)
    }

    /// A clone-able, hook-free summary for recording (the `PreIngressHook`
    /// is not recordable).
    pub fn summary(&self) -> BindSummary {
        BindSummary {
            addr: self.addr,
            queue_max: self.queue_max,
            reuse_port: self.reuse_port,
            roles: self.effective_roles(),
            has_pre_ingress: self.pre_ingress.is_some(),
        }
    }
}

/// Recordable, hook-free projection of [`BindUdpOpts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindSummary {
    pub addr: SocketAddr,
    pub queue_max: usize,
    pub reuse_port: bool,
    pub roles: HashSet<UaRole>,
    pub has_pre_ingress: bool,
}

/// A packet the simulated fabric could not deliver (no endpoint bound at the
/// destination). Surfaced by `drain_undeliverable` and the layer-close audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndeliveredPacket {
    pub raw: Vec<u8>,
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub timestamp_ms: u64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindErrorReason {
    AlreadyBound,
    OsError,
}

/// Failure binding a UDP endpoint (port of `BindError`).
#[derive(Debug, Clone, thiserror::Error)]
#[error("bind {addr} failed ({reason:?}): {message}")]
pub struct BindError {
    pub reason: BindErrorReason,
    pub addr: SocketAddr,
    pub message: String,
}

/// Failure sending a datagram (port of `SendError`).
#[derive(Debug, Clone, thiserror::Error)]
#[error("send failed: {message}")]
pub struct SendError {
    pub message: String,
}

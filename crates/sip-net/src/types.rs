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

use sip_clock::Clock;

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
    /// Arrival timestamp (ms) on the binding endpoint's [`BindUdpOpts::clock`]
    /// timeline. Behavioural input — the proxy ages dequeued packets off it to
    /// drive the self-gate's ELU arm — so an age MUST be taken against that
    /// same monotonic-anchored `Clock`: differencing it against a raw
    /// `SystemTime` reading folds unbounded wall-vs-monotonic drift into it.
    pub arrival_ms: u64,
}

/// How an inbound datagram fared at the receiving endpoint's inbox, recorded on
/// `SignalingNetworkEvent::RecvItem` at DELIVERY time so
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
    /// Demuxed to the CALL (token/dialog correlation succeeded) but no live
    /// logical endpoint accepted it — a picker miss or an endpointless slot
    ///. Rendered on the `ip:port#noendpoint` sub-lane;
    /// like an orphan it is ladder-only, never judged by the audit.
    Unrouted,
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

/// Why an OUTBOUND datagram was re-emitted — the send-side twin of
/// [`RecvDisposition`]. The loadgen's retransmit engine (`--auto-retransmit`)
/// sends these BELOW the recording decorator (straight to the socket), so
/// without a [`SendTap`] they are invisible on the ladder even though they are
/// real frames on the wire. Projection-only: like the modeled-loss / absorbed
/// receive markers, a re-emit is never judged by the RFC audit (a re-emit is a
/// byte-identical retransmission the rules already dedup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReEmitKind {
    /// A proactive Timer A/E/G retransmit of our own request or final response,
    /// still waiting for its response/ACK.
    Retransmit,
    /// Our ACK to an INVITE 2xx, re-sent because the peer retransmitted the 2xx
    /// (our first ACK was lost) — RFC 3261 §13.2.2.4.
    ReAck,
    /// Our non-INVITE response, re-sent because the peer retransmitted the
    /// request (our first response was lost).
    ReAnswer,
}

impl ReEmitKind {
    /// Short human tag for renderers (label suffix / badge text).
    pub fn tag(self) -> &'static str {
        match self {
            ReEmitKind::Retransmit => "re-emit: timer",
            ReEmitKind::ReAck => "re-ACK: dup 2xx",
            ReEmitKind::ReAnswer => "re-answer: dup req",
        }
    }
}

/// Send-time tap installed on an endpoint by the recording decorator (sampled
/// calls only). Invoked with the raw bytes, destination, and [`ReEmitKind`]
/// each time the endpoint's retransmit engine puts a re-emitted datagram on the
/// wire — the outbound twin of [`RecvTap`], so recovery traffic is visible on
/// the ladder rather than silently hitting the socket below the recording.
pub type SendTap = Arc<dyn Fn(&[u8], SocketAddr, ReEmitKind) + Send + Sync>;

/// Per-endpoint counters. Snapshot of the live atomics behind an endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UdpEndpointCounters {
    pub enqueued: u64,
    pub tail_dropped: u64,
    pub pre_ingress_dropped: u64,
    pub pre_ingress_replies: u64,
    /// Pre-ingress replies the socket refused to send (see [`SendErrorKind`]).
    pub pre_ingress_reply_failures: u64,
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
    /// Logical sub-lane label for recording: when
    /// several LOGICAL endpoints share one socket (loadgen mux legs — callee,
    /// alt), the recording decorator keys this bind's lane
    /// `"ip:port#<label>"` instead of the bare `ip:port`, so each leg is its
    /// own ladder column instead of all legs collapsing onto the socket.
    /// Ignored by the transports; recording-only.
    pub lane_label: Option<String>,
    /// Timeline this endpoint stamps [`UdpPacket::arrival_ms`] on. Share the
    /// process `Clock` with whoever ages those packets: two `Clock`s differ by
    /// a constant, a raw wall reading diverges from one without bound.
    pub clock: Clock,
}

impl BindUdpOpts {
    /// Minimal opts: an address and a bounded inbound queue, no pre-ingress
    /// hook, default roles, own arrival clock.
    pub fn new(addr: SocketAddr, queue_max: usize) -> Self {
        Self {
            addr,
            queue_max,
            pre_ingress: None,
            reuse_port: false,
            roles: None,
            lane_label: None,
            clock: Clock::system(),
        }
    }

    /// Stamp arrivals on `clock` (see the `clock` field) — the bind seam a
    /// consumer that ages packets uses to sit on one timeline with them.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Declare the logical sub-lane label (see the `lane_label` field).
    pub fn with_lane_label(mut self, label: impl Into<String>) -> Self {
        self.lane_label = Some(label.into());
        self
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
    /// The simulated fabric already has an endpoint at this address.
    AlreadyBound,
    /// The OS holds the address for another socket (`EADDRINUSE`). Transient
    /// during a same-node process handover — callers may wait for release.
    AddrInUse,
    /// Any other OS-level bind failure.
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

impl BindError {
    /// True when the address is currently held by another socket — the one
    /// bind failure that is retryable (the holder may be a predecessor process
    /// still draining). Structured so callers never string-match the message.
    pub fn is_addr_in_use(&self) -> bool {
        matches!(self.reason, BindErrorReason::AlreadyBound | BindErrorReason::AddrInUse)
    }
}

/// Why a datagram did not leave — the structural axis, so a caller never
/// string-matches an OS message to tell an oversize message from a dead peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SendErrorKind {
    /// `EMSGSIZE`: the datagram exceeds what this path accepts whole. On a
    /// socket pinned to fragment (ADR-0027) this means the message is over
    /// [`MAX_UDP_PAYLOAD`], not merely over the path MTU.
    MessageTooLong,
    /// `ENETUNREACH` / `EHOSTUNREACH` / `ECONNREFUSED`: no route to the peer,
    /// or the peer's port answered with an ICMP rejection.
    Unreachable,
    /// Everything else — buffer exhaustion, a filter's `EPERM`, a closed fd.
    #[default]
    Other,
}

impl SendErrorKind {
    /// Classify an OS send failure by its errno. An error carrying no errno
    /// (a simulated fabric's refusal) is [`Self::Other`].
    pub fn of(err: &std::io::Error) -> Self {
        match err.raw_os_error() {
            Some(libc::EMSGSIZE) => SendErrorKind::MessageTooLong,
            Some(libc::ENETUNREACH | libc::EHOSTUNREACH | libc::ECONNREFUSED) => {
                SendErrorKind::Unreachable
            }
            _ => SendErrorKind::Other,
        }
    }

    /// The metric-label spelling.
    pub const fn label(self) -> &'static str {
        match self {
            SendErrorKind::MessageTooLong => "message_too_long",
            SendErrorKind::Unreachable => "unreachable",
            SendErrorKind::Other => "other",
        }
    }
}

/// Failure sending a datagram (port of `SendError`).
#[derive(Debug, Clone, thiserror::Error)]
#[error("send failed: {message}")]
pub struct SendError {
    pub message: String,
    /// Why it failed, structurally — [`SendErrorKind::Other`] where the sender
    /// has no errno to classify.
    pub kind: SendErrorKind,
}

impl SendError {
    /// A send failure carrying only a reason string — the simulated fabric's
    /// shape, where no errno exists.
    pub fn stated(message: impl Into<String>) -> Self {
        Self { message: message.into(), kind: SendErrorKind::Other }
    }
}

impl From<std::io::Error> for SendError {
    fn from(err: std::io::Error) -> Self {
        Self { message: err.to_string(), kind: SendErrorKind::of(&err) }
    }
}

#[cfg(test)]
mod send_error_tests {
    use super::*;

    fn os(errno: i32) -> SendError {
        SendError::from(std::io::Error::from_raw_os_error(errno))
    }

    /// An oversize datagram is told apart from a dead peer by errno, never by
    /// the OS message text (ADR-0027 X3).
    #[test]
    fn the_errno_names_the_failure_not_the_message_text() {
        assert_eq!(os(libc::EMSGSIZE).kind, SendErrorKind::MessageTooLong);
        assert_eq!(os(libc::EHOSTUNREACH).kind, SendErrorKind::Unreachable);
        assert_eq!(os(libc::ENETUNREACH).kind, SendErrorKind::Unreachable);
        assert_eq!(os(libc::ECONNREFUSED).kind, SendErrorKind::Unreachable);
        assert_eq!(os(libc::ENOBUFS).kind, SendErrorKind::Other);
    }

    /// A failure with no errno behind it — the simulated fabric's refusal —
    /// classifies as `Other` rather than guessing.
    #[test]
    fn a_stated_refusal_carries_no_errno_claim() {
        assert_eq!(SendError::stated("no route in the fabric").kind, SendErrorKind::Other);
        assert_eq!(
            SendError::from(std::io::Error::other("not an OS failure")).kind,
            SendErrorKind::Other
        );
    }

    #[test]
    fn every_kind_has_a_distinct_metric_label() {
        let labels = [
            SendErrorKind::MessageTooLong.label(),
            SendErrorKind::Unreachable.label(),
            SendErrorKind::Other.label(),
        ];
        assert_eq!(labels.len(), std::collections::HashSet::from(labels).len());
    }
}

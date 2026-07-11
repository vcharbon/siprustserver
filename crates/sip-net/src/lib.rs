//! sip-net — the SIP signaling network layer (slice 2 of the migration; port
//! of `src/sip/{SignalingNetwork,UdpTransport}` core).
//!
//! The DI seam is the [`SignalingNetwork`] trait ([`net`]). Implementations:
//!   - [`RealSignalingNetwork`] — `tokio::net::UdpSocket`-backed ([`real`]).
//!   - [`SimulatedSignalingNetwork`] — in-memory routing fabric ([`simulated`]).
//!
//! Recording + auditing is a **decorator** ([`contracts`]) that wraps either
//! impl with the typed `layer-harness` `Recorder` channel and the per-bind /
//! cross-message RFC-rule checks (the source's `scopedAudit`), plus the
//! caller-side `paranoidInputs` precondition decorator. See the
//! `effect-layer-test` SKILL for the wrapper philosophy.

pub mod contracts;
pub mod net;
pub mod queue;
pub mod real;
pub mod report;
pub mod rfc_audit;
pub mod simulated;
pub mod types;

pub use contracts::{
    audit_visible_event, with_all_contracts, CrossMessageAuditRule, ParanoidSignalingNetwork, PeerAuditRule,
    RecordingSignalingNetwork, ScopedAuditOptions, SendOutcome, SignalingAuditViolation,
    SignalingNetworkEvent, WrappedNetwork, SIGNALING_TAG,
};
pub use rfc_audit::{
    bind_roles_of, evaluate_rfc_findings, rfc_cross_message_rules, rfc_peer_rules,
    CSeqInDialogOrderRule, RfcFinding,
};
pub use net::{SignalingNetwork, UdpEndpoint};
pub use report::{to_sip_entries, RecordedSipEntry, RecvNote};
pub use real::RealSignalingNetwork;
pub use simulated::SimulatedSignalingNetwork;
pub use types::{
    all_ua_roles, BindError, BindErrorReason, BindSummary, BindUdpOpts, PreIngressAction,
    PreIngressHook, RecvDisposition, RecvTap, SendError, UaRole, UdpEndpointCounters,
    UdpPacket, UndeliveredPacket,
    MAX_UDP_PAYLOAD,
};

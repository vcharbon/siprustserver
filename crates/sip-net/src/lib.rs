//! sip-net — the SIP signaling network layer.
//!
//! The DI seam is the [`SignalingNetwork`] trait ([`net`]). Implementations:
//!   - [`RealSignalingNetwork`] — `tokio::net::UdpSocket`-backed ([`real`]).
//!   - [`SimulatedSignalingNetwork`] — in-memory routing fabric ([`simulated`]).
//!
//! Recording + auditing is a **decorator** ([`contracts`]) that wraps either
//! impl with the typed `layer-harness` `Recorder` channel and the RFC audit
//! over the recording ([`rfc_audit`], whose rule bodies live once in
//! `rfc-rules`), plus the caller-side precondition decorator
//! [`ParanoidSignalingNetwork`]. See the `effect-layer-test` SKILL for the
//! wrapper philosophy.

pub mod contracts;
pub mod fragmentation;
pub mod loss;
pub mod net;
pub mod queue;
pub mod real;
pub mod repeat;
pub mod report;
pub mod rfc_audit;
pub mod simulated;
pub mod types;

pub use contracts::{
    audit_visible_event, with_all_contracts, CrossMessageAuditRule, ParanoidSignalingNetwork,
    PeerAuditRule, RecordingSignalingNetwork, ScopedAuditOptions, SendOutcome,
    SignalingAuditViolation, SignalingNetworkEvent, WireStamp, WrappedNetwork, SIGNALING_TAG,
};
pub use loss::RandomLoss;
pub use net::{SignalingNetwork, UdpEndpoint};
pub use real::RealSignalingNetwork;
pub use report::{to_sip_entries, wire_positions_by_stamp, RecordedSipEntry, RecvNote};
pub use rfc_audit::{
    audit_wire_entries, bind_roles_of, evaluate_rfc_findings, rfc_cross_message_rules, RfcFinding,
};
pub use simulated::SimulatedSignalingNetwork;
pub use types::{
    all_ua_roles, BindError, BindErrorReason, BindSummary, BindUdpOpts, PreIngressAction,
    PreIngressHook, ReEmitKind, RecvDisposition, RecvTap, SendError, SendErrorKind, SendTap,
    UaRole, UdpEndpointCounters, UdpPacket, UndeliveredPacket, MAX_UDP_PAYLOAD,
};

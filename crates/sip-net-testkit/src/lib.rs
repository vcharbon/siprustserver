//! sip-net-testkit — the test support of the SIP signaling network layer.
//!
//! Every type here implements or wraps the
//! [`SignalingNetwork`](sip_net::SignalingNetwork) seam:
//!   - [`SimulatedSignalingNetwork`] — in-memory routing fabric ([`simulated`]).
//!   - [`RecordingSignalingNetwork`] — the decorator ([`contracts`]) that wraps
//!     either impl with the typed `layer-harness` `Recorder` channel and the
//!     RFC audit over the recording ([`rfc_audit`], whose rule bodies live once
//!     in `rfc-rules`), plus the caller-side precondition decorator
//!     [`ParanoidSignalingNetwork`]. See the `effect-layer-test` SKILL for the
//!     wrapper philosophy.
//!   - [`report`] — the wire-trace projection of a recording; [`repeat`] — the
//!     RFC 3261 §17.2 repeat classification; [`RandomLoss`] — the fabric's
//!     random-loss model.
//!
//! The crate declares no purity tier (ADR-0051 §1): a production binary that
//! links it fails the closure check.

pub mod contracts;
pub mod loss;
pub mod repeat;
pub mod report;
pub mod rfc_audit;
pub mod simulated;

// sip-net's modules, imported under the paths this crate's modules read them by.
use sip_net::{net, pre_ingress, queue, types};

pub use contracts::{
    audit_visible_event, with_all_contracts, CrossMessageAuditRule, ParanoidSignalingNetwork,
    PeerAuditRule, RecordingSignalingNetwork, ScopedAuditOptions, SendOutcome,
    SignalingAuditViolation, SignalingNetworkEvent, WireStamp, WrappedNetwork, SIGNALING_TAG,
};
pub use loss::RandomLoss;
pub use report::{to_sip_entries, wire_positions_by_stamp, RecordedSipEntry, RecvNote};
pub use rfc_audit::{
    audit_wire_entries, bind_roles_of, evaluate_rfc_audit, evaluate_rfc_findings,
    rfc_cross_message_rules, RfcAudit, RfcFinding,
};
pub use simulated::{SendFault, SimulatedSignalingNetwork};

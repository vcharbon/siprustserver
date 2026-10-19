//! The decision seam's values and folds (ADR-0052 §7): the DTOs a consult
//! sends and answers with, the error a consult that gave no decision states,
//! and the folds that apply an answer to the call — the route up to its admit
//! and dialled ([`route_fold`]), the limiter-reject failover
//! ([`failover_fold`]), the reject and redirect ([`apply_reject`]). The
//! consults themselves are the shell's.

pub mod apply_reject;
mod error;
pub mod failover_fold;
mod no_answer;
pub mod route_fold;
mod schemas;

pub use b2bua_sdk::header_update::{header_lines, payload_lines, HeaderUpdate, SipHeaderUpdates};
pub use error::CallDecisionError;
pub use no_answer::no_answer_deadline;
pub use route_fold::MAX_LIMITER_FAILOVER;
pub use schemas::{
    default_platform_features, read_stated_port, BodyUpdate, CallFailureRequest,
    CallFailureResponse, CallReferRequest, CallReferResponse, CallReleaseRequest,
    CallReleaseResponse, CallSnapshot, CallTreatment, FailureInfo, FeatureActivations, LegSnapshot,
    NewCallRequest, NewCallResponse, RedirectContact, RedirectDecision, RejectDecision,
    RouteDecision, SipDestination,
};

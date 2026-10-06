//! # b2bua-sdk — the public Rule SDK (ADR-0016 X6)
//!
//! The minimal, dogfood-driven surface a **callflow service** is authored
//! against. It is a *lower* crate: `b2bua` depends on `b2bua-sdk`, never the
//! reverse, so an out-of-tree service crate (e.g. `announcement`) can
//! build a per-call state machine while depending on **only** `b2bua-sdk` — it
//! has no path to `b2bua`'s internals.
//!
//! What lives here is the authoring vocabulary, not the engine:
//! - the [`define_service!`] / [`sm_rule!`] macros (the declarative authoring DSL);
//! - the rule-engine model — [`Match`](model::Match), [`RuleAction`](model::RuleAction),
//!   [`RuleDefinition`](model::RuleDefinition), [`RuleContext`](model::RuleContext);
//! - the service registry types [`ServiceSeed`](service::ServiceSeed) /
//!   [`ServiceDef`](service::ServiceDef);
//! - the inputs a rule reads — [`CallEvent`] and
//!   [`B2buaConfig`].
//!
//! The engine that *runs* these (the `ActionExecutor`, the invariant enforcer,
//! the dispatcher, the composition glue `compose_rules`/`seed_services`) stays
//! in `b2bua`. The boundary is realised with **curated re-exports** of the
//! internal `RuleAction` (a soft boundary, less glue) rather than a distinct
//! mapped SDK type (ADR-0016); the `announcement` crate compiles against this
//! surface alone.

pub mod body;
pub mod config;
pub mod event;
pub mod failure_image;
pub mod fold_payload;
pub mod header_update;
pub mod in_dialog_relay;
pub mod model;
pub mod open_offer;
pub mod provisional;
pub mod reason_phrase;
pub mod relayed_final;
pub mod release_reason;
pub mod sdp_form;
pub mod service;

/// The framework-type façade the [`define_service!`] / [`sm_rule!`] macros
/// reference through `$crate::rules::…`, and the curated surface a service crate
/// imports (`use b2bua_sdk::rules::*`).
pub mod rules {
    pub use crate::body::{Body, BodyAuthor};
    pub use crate::model::{
        Effect, EffectKind, GlareRefusal, Match, MatchKind, MessageTransform, RuleAction, RuleCall,
        RuleContext, RuleDefinition, RuleHandleResult, StatusMatch, TimerDelay, CORE_LAYER,
        SERVICE_LAYER,
    };
    pub use crate::provisional::{
        absorbed_provisional_actions, originator_final_sent, reliable_rseq,
    };
    pub use crate::relayed_final::RelayedFinal;
    pub use crate::service::{ServiceDef, ServiceSeed, Terminal};
    pub use sip_message::draft::Entry;
    pub use sip_message::header::HeaderName;
    pub use sip_message::Method;
    // NOTE: `Call` is deliberately NOT re-exported here (ADR-0020 X8) — rules
    // read through the [`RuleCall`](crate::model::RuleCall) view. The full
    // struct stays nameable only as `b2bua_sdk::service::Call`, for the
    // `ServiceSeed::data_write` installer.
    pub use call::{MachineId, StateLabel, TerminationCause, TimeoutKind};
}

pub use config::B2buaConfig;
pub use event::CallEvent;
pub use sdp_form::{AsWritten, SdpCrossing, SdpForm, SdpFormPolicy};

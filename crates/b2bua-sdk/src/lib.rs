//! # b2bua-sdk — the public Rule SDK (ADR-0016 X6)
//!
//! The minimal, dogfood-driven surface a **callflow service** is authored
//! against. It is a *lower* crate: `b2bua` depends on `b2bua-sdk`, never the
//! reverse, so an out-of-tree service crate (e.g. `announcement`, slice 8) can
//! build a per-call state machine while depending on **only** `b2bua-sdk` — it
//! has no path to `b2bua`'s internals.
//!
//! What lives here is the authoring vocabulary, not the engine:
//! - the [`define_service!`] / [`sm_rule!`] macros (the declarative authoring DSL);
//! - the rule-engine model — [`Match`](model::Match), [`RuleAction`](model::RuleAction),
//!   [`RuleDefinition`](model::RuleDefinition), [`RuleContext`](model::RuleContext);
//! - the service registry types [`ServiceSeed`](service::ServiceSeed) /
//!   [`ServiceDef`](service::ServiceDef);
//! - the inputs a rule reads — [`CallEvent`](event::CallEvent) and
//!   [`B2buaConfig`](config::B2buaConfig).
//!
//! The engine that *runs* these (the `ActionExecutor`, the invariant enforcer,
//! the dispatcher, the composition glue `compose_rules`/`seed_services`) stays
//! in `b2bua`. Per the slice-6 sub-decision (ADR-0016) the boundary is realised
//! with **curated re-exports** of the internal `RuleAction` (a soft boundary,
//! less glue) rather than a distinct mapped SDK type; the validation is slice 8,
//! whose `announcement` crate must compile against this surface alone.

pub mod config;
pub mod event;
pub mod model;
pub mod service;

/// The framework-type façade the [`define_service!`] / [`sm_rule!`] macros
/// reference through `$crate::rules::…`, and the curated surface a service crate
/// imports (`use b2bua_sdk::rules::*`).
pub mod rules {
    pub use crate::model::{
        Effect, EffectKind, Match, MatchKind, MessageTransform, RuleAction, RuleContext,
        RuleDefinition, RuleHandleResult, StatusMatch, CORE_LAYER, SERVICE_LAYER,
    };
    pub use sip_message::Method;
    pub use crate::service::{ServiceDef, ServiceSeed, Terminal};
    pub use call::{Call, MachineId, StateLabel};
}

pub use config::B2buaConfig;
pub use event::CallEvent;

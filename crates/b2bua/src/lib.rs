//! `b2bua` — the B2BUA core (migration slice "Dispatch / per-call FIFO" +
//! "Rule engine"; port of `src/sip/{SipRouter,PerCallDispatcher}.ts`,
//! `src/call/CallState.ts`, `src/b2bua/rules/`, and `src/decision/`).
//!
//! Layers, bottom-up:
//! - [`store`] — the in-memory call map + per-call serialization over a
//!   replication-aware [`store::CallStore`] seam (HA drops in later, no changes
//!   to rules/dispatch).
//! - [`dispatch`] — the per-call FIFO: a bounded queue + worker task per call,
//!   capped globally (port of `PerCallDispatcher`, source ADR-0004/0005).
//! - [`timers`] — one `DelayQueue` driver firing [`event::CallEvent::Timer`].
//! - [`decision`] — the call-decision adapter seam + a scripted test impl.
//! - [`rules`] — first-match, layer-ranked rule engine + invariant enforcement.
//! - [`router`] — consumes the transaction-layer event stream, resolves the
//!   `callRef`, runs the handler, interprets the typed [`effects`].
//! - [`b2bua_core`] — wires it all together.
//!
//! Builds on the already-ported `call` data model; see MIGRATION_STATUS + ADR-0010.

pub mod b2bua_core;
pub mod cdr;
pub mod config;
pub mod decision;
pub mod dispatch;
pub mod drain;
pub mod effects;
pub mod event;
pub mod initial_invite;
pub mod limiter;
pub mod limiter_http;
pub mod metrics;
pub mod obligations;
pub mod overload;
pub mod reaper;
pub mod repl;
pub mod router;
pub mod rules;
pub mod stack_identity;
pub mod store;
pub mod target_admission;
pub mod timers;

pub use b2bua_core::{B2buaCore, B2buaDeps, ReplicationSetup};

pub use config::B2buaConfig;
pub use effects::{HandlerEffects, HandlerResult};
pub use event::CallEvent;
pub use metrics::B2buaMetrics;
// The callflow-service authoring macros live in the public Rule SDK (ADR-0016
// slice 6); re-export them so in-tree services keep using `b2bua::define_service!`
// / `b2bua::sm_rule!`. (`$crate` inside the macro resolves to `b2bua_sdk`, where
// the SDK vocabulary the expansion references lives.)
pub use b2bua_sdk::{define_service, sm_rule};

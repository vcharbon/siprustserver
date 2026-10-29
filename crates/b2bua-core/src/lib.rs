//! `b2bua-core` — the B2BUA's decision core (ADR-0052): what a call's turn
//! decides, as a function of its inputs. The shell (`b2bua`) owns every
//! effect: the router, the store, replication, the timers, the limiter
//! worker, the decision engines, the CDR writer, the metrics and the trace
//! registry. It hands each turn the per-process values, the call, the event
//! as it resolved it, the caller's INVITE, the turn's clock reading, the wire
//! faults armed at its start and the turn's identifier source, and drains
//! what the turn notes.
//!
//! - [`rules`] — the rule engine and every rule family, the action executor
//!   and vocabulary, the invariants, relay construction;
//! - [`rule_chain`] — the per-turn framework around the rules;
//! - [`dialog_gate`] — what the dialog gate makes of a message before any
//!   rule reads it;
//! - [`landing`] — the pass a turn's result takes before the shell persists it;
//! - [`initial_invite`], [`decision`] — the initial INVITE's folds and the
//!   decision DTOs;
//! - [`effects`] — the effect vocabulary a turn returns;
//! - [`unheld_response`] — the answer to a response for a call no node holds;
//! - [`turn_scope`], [`turn_notes`], [`wire_fault_set`], [`owners`] — a turn's
//!   inputs and what it reports.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::iter_over_hash_type,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

pub mod answer_deadline;
pub mod answered_finals;
pub mod attempts;
pub use b2bua_sdk::caller_tags;
pub mod config;
pub mod decision;
pub mod decision_log;
pub mod destination_allowlist;
pub mod dialog_gate;
pub mod effects;
pub mod failure_terminate;
pub mod failure_unavailable;
pub mod initial_invite;
pub mod landing;
pub mod limiter;
pub mod message_ring;
pub mod obligations;
pub mod owners;
pub mod reaper_verdict;
pub mod ring_deadline;
pub mod rule_chain;
#[cfg(any(test, feature = "testkit"))]
pub mod rule_fixture;
pub mod rules;
pub mod stack_identity;
#[cfg(any(test, feature = "testkit"))]
pub mod test_call;
pub mod timer_arm;
#[cfg(any(test, feature = "testkit"))]
pub mod turn_driver;
pub mod turn_notes;
pub mod turn_scope;
pub mod unheld_response;
pub mod wire_fault_set;

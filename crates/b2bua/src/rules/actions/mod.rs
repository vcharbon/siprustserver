//! `ActionExecutor` — translates a rule's [`RuleAction`]s into a
//! [`HandlerResult`] (updated [`Call`] + typed [`HandlerEffects`]), scoped to
//! the basic-B2BUA action set. State mutations use the `call`-crate lens
//! helpers; outbound messages use the [`crate::rules::relay`] primitives. One
//! module per action family:
//!
//! - [`dispatch`] — the `RuleAction` → handler table (+ inline state mutations)
//! - [`relay_request`] / [`relay_response`] — relaying the current event to a peer
//! - [`dialog_track`] — early-dialog tracking, 2xx confirmation, the a-dialog
//! - [`originate`] — requests the B2BUA sends itself (CreateLeg, re-INVITE,
//!   NOTIFY, PRACK, in-dialog probes)
//! - [`respond`] — response synthesis toward a leg (finals, provisionals)
//! - [`ladder`] — the dialog-level retransmission ladders the framework owns
//!   (ADR-0029 X4): arm / repeat / discharge / retire, keyed by obligation
//! - [`teardown`] — termination policy + BYE/CANCEL builders
//! - [`select`] — shared leg/dialog selection views

mod dialog_track;
mod dispatch;
mod ladder;
mod originate;
mod relay_request;
mod relay_response;
mod respond;
mod select;
mod teardown;

use call::helpers::{cap_keepalive_fire_at, replace_timer_by_id};
use call::{Call, TimerEntry, TimerType};
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult};

use super::model::{RuleAction, RuleContext};

/// Executes rule actions against a working copy of the call.
pub struct ActionExecutor<'a> {
    pub config: &'a B2buaConfig,
    pub id_gen: &'a IdGen,
    pub now_ms: i64,
    /// The wire-fault seam; consulted where a guarded emission is built.
    pub wire_faults: &'a crate::wire_faults::WireFaults,
}

impl ActionExecutor<'_> {
    /// Apply `actions` to a working copy of the authoritative `call`. The
    /// `ctx` view carries the event; the full struct comes in explicitly —
    /// rules never hold it (ADR-0020 X8).
    pub fn execute(&self, actions: &[RuleAction], call: &Call, ctx: &RuleContext) -> HandlerResult {
        let mut call = call.clone();
        let mut fx = HandlerEffects::new();
        for action in actions {
            self.apply(action, ctx, &mut call, &mut fx);
        }
        HandlerResult { call, effects: fx }
    }

    /// Arm (or re-arm) a persisted per-call timer. The ONE persisted-id recipe
    /// (`TimerType::timer_id`); every cancel site mints from the same method so
    /// schedule/cancel can never drift. A `Keepalive` is additionally held to the
    /// ledger's one-interval ceiling ([`cap_keepalive_fire_at`]) — the arming half
    /// of the invariant `router::restore_hygiene` enforces on hydrated deadlines.
    fn schedule(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        timer_type: TimerType,
        delay_ms: i64,
        leg_id: Option<String>,
    ) {
        let id = timer_type.timer_id(leg_id.as_deref());
        let entry = TimerEntry {
            id,
            fire_at: self.capped_fire_at(&timer_type, self.now_ms + delay_ms),
            timer_type,
            leg_id,
        };
        call.timers = replace_timer_by_id(std::mem::take(&mut call.timers), entry.clone());
        fx.critical.push(CriticalStateEffect::ScheduleTimer(entry));
    }

    /// `fire_at` for a minted timer, with a `Keepalive` held to one keepalive
    /// interval (the configured cadence — every arming site re-arms at exactly
    /// that, so a farther deadline is a defect at the caller and trips here in
    /// debug builds). Non-keepalive deadlines pass through: a policy timer
    /// legitimately outlives a probe cadence.
    fn capped_fire_at(&self, timer_type: &TimerType, fire_at: i64) -> i64 {
        if !matches!(timer_type, TimerType::Keepalive) {
            return fire_at;
        }
        let interval_ms = self.config.keepalive_interval_sec * 1000;
        let capped = cap_keepalive_fire_at(fire_at, self.now_ms, interval_ms);
        debug_assert_eq!(
            capped,
            fire_at,
            "Keepalive armed beyond one interval: {} ms out of a {interval_ms} ms cadence",
            fire_at - self.now_ms,
        );
        capped
    }
}

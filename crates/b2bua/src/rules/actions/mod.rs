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
//! - [`respond`] — response synthesis toward a leg (finals, provisionals,
//!   un-ACKed-2xx retransmits)
//! - [`teardown`] — termination policy + BYE/CANCEL builders
//! - [`select`] — shared leg/dialog selection views

mod dialog_track;
mod dispatch;
mod originate;
mod relay_request;
mod relay_response;
mod respond;
mod select;
mod teardown;

use call::helpers::replace_timer_by_id;
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
}

impl ActionExecutor<'_> {
    /// Apply `actions` to a working copy of the authoritative `call`. The
    /// `ctx` view carries the event; the full struct comes in explicitly —
    /// rules never hold it (ADR-0020 X8).
    pub fn execute(
        &self,
        actions: &[RuleAction],
        call: &Call,
        ctx: &RuleContext,
    ) -> HandlerResult {
        let mut call = call.clone();
        let mut fx = HandlerEffects::new();
        for action in actions {
            self.apply(action, ctx, &mut call, &mut fx);
        }
        HandlerResult { call, effects: fx }
    }

    /// Arm (or re-arm) a persisted per-call timer. The ONE persisted-id recipe
    /// (`TimerType::timer_id`); every cancel site mints from the same method so
    /// schedule/cancel can never drift.
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
            timer_type,
            fire_at: self.now_ms + delay_ms,
            leg_id,
        };
        call.timers = replace_timer_by_id(std::mem::take(&mut call.timers), entry.clone());
        fx.critical.push(CriticalStateEffect::ScheduleTimer(entry));
    }
}

//! `ActionExecutor` — translates a rule's [`RuleAction`]s into a
//! [`HandlerResult`] (updated [`Call`] + typed [`HandlerEffects`]), scoped to
//! the basic-B2BUA action set. State mutations use the `call`-crate lens
//! helpers; outbound messages use the [`crate::rules::relay`] primitives. One
//! module per action family:
//!
//! - `dispatch` — the `RuleAction` → handler table (+ inline state mutations)
//! - `ack_leg` — the ACK of a 2xx toward a leg, relayed or this stack's own
//! - `relay_request` / `relay_response` — relaying the current event to a peer
//! - `dialog_track` — early-dialog tracking, 2xx confirmation, the a-dialog
//! - `dialog_end` — a leg's dialogs moving to the state its teardown records
//! - `create_leg` — the new leg's INVITE, opening the attempt it dials
//! - `originate` — requests the B2BUA sends itself (re-INVITE,
//!   NOTIFY, in-dialog probes)
//! - `prack` — the PRACKs this stack owes responders, settled in one step
//! - `provisional_arrival` — the engine's record of a reliable provisional
//!   as it arrives, before the rules
//! - `respond` — response synthesis toward a leg (finals, provisionals)
//! - `ladder` — the dialog-level retransmission ladders the framework owns
//!   (ADR-0032 X4): arm / repeat / discharge, keyed by obligation
//! - `retire` — leaving a scope: its ladders end and its timers are disarmed
//! - `teardown` — termination policy + BYE/CANCEL builders
//! - `reinvite_cancel` — the transaction-scoped CANCEL of a relayed re-INVITE
//! - `select` — shared leg/dialog selection views
//! - `straggler` — the ACK and BYE a fork straggler's 2xx draws

mod ack_leg;
mod confirm;
mod create_leg;
mod dialog_end;
mod dialog_track;
mod dispatch;
mod ladder;
mod originate;
mod prack;
mod provisional_arrival;
mod reinvite_cancel;
mod relay_request;
mod relay_response;
mod respond;
mod retire;
mod select;
mod straggler;
mod teardown;
mod unrelayed;

use call::helpers::{cap_keepalive_fire_at, disarm_timer, retire_timers};
use call::{Call, TimerEntry, TimerKey, TimerType};
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult};

use b2bua_sdk::model::{RuleAction, RuleContext};

pub use prack::{Owed, Settle};

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

    /// Arm (or re-arm) the persisted per-call timer `key`, replacing the
    /// instance it held under the call's next generation. A `Keepalive` is
    /// additionally held to the ledger's one-interval ceiling
    /// ([`cap_keepalive_fire_at`]) — the arming half of the invariant
    /// `router::restore_hygiene` enforces on hydrated deadlines.
    fn schedule(&self, call: &mut Call, fx: &mut HandlerEffects, key: TimerKey, delay_ms: i64) {
        let fire_at = self.capped_fire_at(&key.timer_type, self.now_ms + delay_ms);
        crate::timer_arm::arm(call, fx, key, fire_at);
    }

    /// Disarm `key`: off the replicated ledger and the live driver. A key the
    /// ledger does not hold is not in the driver either, so nothing is
    /// cancelled twice.
    fn disarm(&self, call: &mut Call, fx: &mut HandlerEffects, key: &TimerKey) {
        if let Some(entry) = disarm_timer(call, key) {
            fx.critical.push(CriticalStateEffect::CancelTimer { key: entry.key });
        }
    }

    /// Remove every ledger entry `spent` selects from the replicated ledger AND
    /// the live driver — one `CancelTimer` per removed key.
    fn scrub_where(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        spent: impl Fn(&TimerEntry) -> bool,
    ) {
        for t in retire_timers(call, spent) {
            fx.critical.push(CriticalStateEffect::CancelTimer { key: t.key });
        }
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

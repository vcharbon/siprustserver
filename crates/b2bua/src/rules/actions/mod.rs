//! `ActionExecutor` — translates a rule's [`RuleAction`]s into a
//! [`HandlerResult`] (updated [`Call`] + typed [`HandlerEffects`]), scoped to
//! the basic-B2BUA action set. State mutations use the `call`-crate lens
//! helpers; outbound messages use the [`crate::rules::relay`] primitives. One
//! module per action family:
//!
//! - `dispatch` — the `RuleAction` → handler table (+ inline state mutations)
//! - `ack_leg` — the ACK of a 2xx toward a leg, relayed or this stack's own
//! - `lost_round` — the ACK of an INVITE final for a round the record lost
//! - `relay_request` / `relay_response` — relaying the current event to a peer
//! - `dialog_track` — early-dialog tracking, 2xx confirmation, the a-dialog
//! - `dialog_end` — a leg's dialogs moving to the state its teardown records
//! - `create_leg` — the new leg's INVITE, opening the attempt it dials
//! - `originate` — requests the B2BUA sends itself (re-INVITE,
//!   NOTIFY, in-dialog probes)
//! - `own_notify` — this stack's requests held behind its own unanswered
//!   NOTIFY on a dialog
//! - `prack` — the PRACKs this stack owes responders, settled in one step
//! - `provisional_arrival` — the engine's record of a reliable provisional
//!   as it arrives, before the rules
//! - `show_provisional` — a reliable provisional shown under this stack's number
//! - `respond` — response synthesis toward a leg (finals, provisionals)
//! - `ladder` — the dialog-level retransmission ladders the framework owns
//!   (ADR-0032 X4): arm / repeat / discharge, keyed by obligation
//! - `retire` — leaving a scope: its ladders end and its timers are disarmed
//! - `teardown` — termination policy + the CANCEL builder
//! - `relayed_teardown` — what a minted BYE or CANCEL carries of the peer
//! - `bye` — the BYE this stack sends on a confirmed dialog
//! - `reinvite_cancel` — the transaction-scoped CANCEL of a relayed re-INVITE
//! - `select` — shared leg/dialog selection views
//! - `straggler` — the ACK and BYE a fork straggler's 2xx draws

mod ack_leg;
mod bye;
mod confirm;
mod create_leg;
mod dialog_end;
mod dialog_track;
mod dispatch;
mod ladder;
mod lost_round;
mod originate;
mod own_notify;
mod prack;
mod provisional_arrival;
mod reinvite_cancel;
mod relay_request;
mod relay_response;
mod relayed_teardown;
mod respond;
mod retire;
mod select;
mod show_provisional;
mod snapshot_answer;
mod straggler;
mod teardown;
mod unrelayed;

use call::helpers::{cap_keepalive_fire_at, disarm_timer, retire_timers};
use call::{Call, TimerEntry, TimerKey, TimerType};

use crate::config::B2buaConfig;
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult};
use crate::turn_scope::TurnScope;

use b2bua_sdk::model::{RuleAction, RuleContext};

pub(crate) use own_notify::holds_requests;
pub use prack::{Owed, Settle};
pub(crate) use provisional_arrival::invite_open;

/// Executes rule actions against a working copy of the call, drawing every
/// identifier it mints from its turn and noting there what it reports.
pub struct ActionExecutor<'a> {
    pub config: &'a B2buaConfig,
    /// The turn the actions run in: its identifier source, its notes and the
    /// wire faults its guarded emissions read.
    pub turn: &'a mut TurnScope,
    pub now_ms: i64,
}

impl ActionExecutor<'_> {
    /// Apply `actions` to a working copy of the authoritative `call`. The
    /// `ctx` view carries the event; the full struct comes in explicitly —
    /// rules never hold it (ADR-0020 X8). An action that breaks ends the list:
    /// the actions after it are not applied.
    pub fn execute(
        &mut self,
        actions: &[RuleAction],
        call: &Call,
        ctx: &RuleContext,
    ) -> HandlerResult {
        let mut call = call.clone();
        let mut fx = HandlerEffects::new();
        for action in actions {
            if self.step(action, ctx, &mut call, &mut fx).is_break() {
                break;
            }
        }
        HandlerResult { call, effects: fx }
    }

    /// Arm (or re-arm) the persisted per-call timer `key`, replacing the
    /// instance it held under the call's next generation. A `Keepalive` is
    /// additionally held to the ledger's one-interval ceiling
    /// ([`cap_keepalive_fire_at`]) — the arming half of the invariant
    /// `router::restore_hygiene` enforces on hydrated deadlines.
    fn schedule(&mut self, call: &mut Call, fx: &mut HandlerEffects, key: TimerKey, delay_ms: i64) {
        let fire_at = self.capped_fire_at(&key.timer_type, self.now_ms + delay_ms);
        crate::timer_arm::arm(call, fx, key, fire_at);
    }

    /// Disarm `key`: off the replicated ledger and the live driver. A key the
    /// ledger does not hold is not in the driver either, so nothing is
    /// cancelled twice.
    fn disarm(&mut self, call: &mut Call, fx: &mut HandlerEffects, key: &TimerKey) {
        if let Some(entry) = disarm_timer(call, key) {
            fx.critical.push(CriticalStateEffect::CancelTimer { key: entry.key });
        }
    }

    /// Remove every ledger entry `spent` selects from the replicated ledger AND
    /// the live driver — one `CancelTimer` per removed key.
    fn scrub_where(
        &mut self,
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
    fn capped_fire_at(&mut self, timer_type: &TimerType, fire_at: i64) -> i64 {
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

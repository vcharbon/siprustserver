//! `ActionExecutor` — translates a rule's [`RuleAction`]s into a
//! [`HandlerResult`] (updated [`Call`] + typed [`HandlerEffects`]), scoped to
//! the basic-B2BUA action set. State mutations use the `call`-crate lens
//! helpers; outbound messages use the [`crate::rules::relay`] primitives. One
//! module per action family:
//!
//! - `dispatch` — the `RuleAction` → handler table (+ inline state mutations)
//! - `record_write` — the actions that write the record or hand a request to a
//!   service or the limiter, with no SIP leaving
//! - `ack_leg` — the ACK of a 2xx toward a leg, relayed or this stack's own
//! - `evidence` — what evidence the record cannot explain is owed, and the
//!   dialog state it proves (ADR-0055 §4)
//! - `possibly_confirmed` — the caller dialog held while it may carry a 2xx
//!   another owner sent (ADR-0055 §4)
//! - `relay_request` / `relay_response` — relaying the current event to a peer
//! - `relay_pending_response` — a response to a request this stack relayed
//! - `request_record` — the non-INVITE request records: written as a request
//!   leaves or is held, settled by its final, timeout, leg end, release or
//!   deadline (ADR-0055 §2)
//! - `dialog_track` — early-dialog tracking, 2xx confirmation, the a-dialog
//! - `dialog_end` — a leg's dialogs moving to the state its teardown records
//! - `create_leg` — the new leg's INVITE, opening the attempt it dials
//! - `originate` — requests the B2BUA sends itself (re-INVITE,
//!   NOTIFY, in-dialog probes)
//! - `subscription` — this stack as the notifier of a dialog's subscriptions:
//!   a NOTIFY refused once its subscription is terminated, and this stack's
//!   requests queued behind its own unanswered NOTIFY
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
mod evidence;
mod ladder;
mod originate;
mod peer_bye;
pub mod possibly_confirmed;
mod prack;
mod provisional_arrival;
mod record_write;
mod reinvite_cancel;
mod relay_pending_response;
mod relay_request;
mod relay_response;
mod relayed_teardown;
mod request_record;
mod respond;
mod retire;
mod select;
mod show_provisional;
mod snapshot_answer;
mod straggler;
mod subscription;
mod teardown;
mod unrelayed;

use call::helpers::{cap_keepalive_fire_at, disarm_timer, retire_timers};
use call::{Call, TimerEntry, TimerKey, TimerType};

use crate::config::B2buaConfig;
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult};
use crate::turn_scope::TurnScope;

use b2bua_sdk::model::{RuleAction, RuleContext};

pub use prack::{Owed, Settle};
pub use provisional_arrival::invite_open;
pub(crate) use request_record::settle_owed;
pub(crate) use subscription::holds_requests;

/// Executes rule actions against a working copy of the call, drawing every
/// identifier it mints from its turn and noting there what it reports.
pub struct ActionExecutor<'a> {
    pub config: &'a B2buaConfig,
    /// The call's owners, as the shell states them for the turn.
    pub owners: &'a crate::owners::Owners,
    /// The turn the actions run in: its identifier source, its notes and the
    /// wire faults its guarded emissions read.
    pub turn: &'a mut TurnScope,
    pub now_ms: i64,
}

impl<'a> ActionExecutor<'a> {
    /// The marks this stack stamps on what it emits for `call`, under the
    /// turn's owners.
    fn marks<'c>(&self, call: &'c Call) -> crate::rules::relay::CallMarks<'c>
    where
        'a: 'c,
    {
        crate::rules::relay::CallMarks::of(call, self.owners)
    }
}

impl ActionExecutor<'_> {
    /// Apply `actions` to a working copy of the authoritative `call`. The
    /// `ctx` view carries the event; the full struct comes in explicitly —
    /// rules never hold it (ADR-0020 X8). An action that breaks ends the list:
    /// the actions after it are not applied. A peer's BYE the actions answer
    /// by ending its leg first draws the ACK its 2xx is owed (RFC 3261
    /// §13.2.2.4).
    pub fn execute(
        &mut self,
        actions: &[RuleAction],
        call: &Call,
        ctx: &RuleContext,
    ) -> HandlerResult {
        let mut call = call.clone();
        let mut fx = HandlerEffects::new();
        self.ack_before_peer_bye_answer(&mut call, &mut fx, ctx, actions);
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
        let fire_at = self.capped_fire_at(call, &key.timer_type, self.now_ms + delay_ms);
        crate::timer_arm::arm(call, fx, key, fire_at);
    }

    /// The caller's INVITE, rebuilt from `call`'s record for the step that
    /// reads it ([`crate::rules::relay::caller_invite`]).
    fn caller_invite(&mut self, call: &Call) -> Option<sip_message::SipRequest> {
        crate::rules::relay::caller_invite(call, &mut self.turn.notes)
    }

    /// Note that the leg `leg_id` an in-dialog `what` was being built for left
    /// `call` while it was built: the request is not sent.
    fn leg_gone(&mut self, call: &Call, leg_id: &str, what: &str) {
        let detail = format!("{what} on leg {leg_id}: the leg left the record");
        self.turn.notes.defect(crate::turn_notes::REQUEST_KEEPS_ITS_LEG, detail, call);
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
    /// that, so a farther deadline is a defect at the caller, noted on the
    /// turn). Non-keepalive deadlines pass through: a policy timer
    /// legitimately outlives a probe cadence.
    fn capped_fire_at(&mut self, call: &Call, timer_type: &TimerType, fire_at: i64) -> i64 {
        if !matches!(timer_type, TimerType::Keepalive) {
            return fire_at;
        }
        let interval_ms = self.config.keepalive_interval_sec * 1000;
        let capped = cap_keepalive_fire_at(fire_at, self.now_ms, interval_ms);
        if capped != fire_at {
            let out = fire_at - self.now_ms;
            let detail = format!("Keepalive armed {out} ms out of a {interval_ms} ms cadence");
            self.turn.notes.defect(crate::turn_notes::KEEPALIVE_WITHIN_CADENCE, detail, call);
        }
        capped
    }
}

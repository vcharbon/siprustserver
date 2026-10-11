//! The dialog-level retransmission ladders the framework owns (ADR-0032 X4):
//! the un-ACKed 2xx (RFC 3261 §13.3.1.4, initial INVITE and re-INVITE alike)
//! and the un-PRACKed reliable provisional (RFC 3262 §3). A ladder is armed
//! under the [`Obligation`] that discharges it, paced by `sip_retransmit`'s
//! class, and keyed into the timer ledger by that obligation — so the same key
//! arms it, repeats it, retires it when the discharging ACK or PRACK arrives,
//! and names the one give-up a rule sees. No rule cancels a ladder timer.
//! The provisionals' books live in `call::helpers`; the relays that emit the
//! originals live in [`super::relay_response`] and [`super::respond`].

use std::time::Duration;

use call::helpers::{RAckTokens, Scope};
use call::{
    CSeq, Call, CallModelState, Ceased, LegState, Obligation, RSeq, Repeated, RetainedEmission,
    TimerKey, TimerType, Unacked2xx,
};
use sip_message::header::RAck;
use sip_message::{Method, SipMessage, SipResponse};
use sip_retransmit::{Class, Schedule};

use crate::effects::{
    HandlerEffects, HandlerResult, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::defaults::unacked_2xx_give_up_actions;
use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::{RuleCall, RuleContext};

use super::ActionExecutor;

impl ActionExecutor<'_> {
    // ── arming ─────────────────────────────────────────────────────────────

    /// Retain the a-leg's INITIAL answer as the datagram it leaves as — its
    /// `image()`, which the end-of-turn stamp rebinds to the stamped bytes
    /// ([`crate::rules::stated_headers`]) and the transaction layer sends verbatim
    /// — and arm its §13.3.1.4 ladder under `AckOf2xx`, keyed by the To-tag and
    /// CSeq the 2xx itself carries — the two facts the caller's ACK echoes. The
    /// a-leg INVITE server transaction goes `Completed` on this final, so the
    /// txn layer neither retransmits the 2xx nor reports the missing ACK: this
    /// ladder is the one that does.
    pub(super) fn retain_a_leg_answer(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        effect: &mut OutboundSipEffect,
    ) {
        let OutboundBody::Response(resp) = &effect.body else {
            return;
        };
        let (unacked, obligation, first) =
            unacked_2xx_of(resp, effect.destination.clone(), &call.a_leg.leg_id);
        if let Some(d) = call::helpers::caller_dialog_mut(call, &unacked.dialog_tag.clone()) {
            d.answered_2xx = Some(unacked);
        }
        self.arm_ack_ladder(call, fx, obligation, first);
    }

    /// Retain a **re-INVITE** 2xx relayed toward its originator (`target_leg`,
    /// either face), on the originator's dialog the 2xx names, and arm its
    /// §13.3.1.4 ladder. The
    /// marker also holds the dialog's INVITE server transaction in RFC 6026
    /// *Accepted* for `reinvite-glare`, for exactly as long as the ladder's
    /// give-up stands.
    pub(super) fn retain_reinvite_2xx(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        resp: &mut SipResponse,
        dest: (String, u16),
        target_leg: &str,
    ) {
        let (unacked, obligation, first) = unacked_2xx_of(resp, dest, target_leg);
        let (ours, peer) =
            (resp.to().tag().map(str::to_string), resp.from().tag().map(str::to_string));
        *call = call::helpers::update_leg(call.clone(), target_leg, |leg| {
            if let Some(d) = ours.zip(peer).and_then(|(o, p)| leg.dialogs.by_tags_mut(&o, &p)) {
                d.pending_reinvite_2xx = Some(unacked);
            }
        });
        self.arm_ack_ladder(call, fx, obligation, first);
    }

    /// Arm a 2xx ladder: the first rung at `first`, the give-up at the
    /// deployment's deadline (`ack_give_up_deadline`). Unconditional (ADR-0032
    /// X5): no configuration reaches the rungs or the give-up — a 2xx that
    /// leaves un-ACKed is repeated, and the deadline that ends its session is
    /// in the ledger for as long as the retained marker is.
    fn arm_ack_ladder(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        obligation: Obligation,
        first: Duration,
    ) {
        self.schedule(
            call,
            fx,
            TimerKey::obligation(TimerType::Rung, obligation.clone()),
            first.as_millis() as i64,
        );
        self.schedule(
            call,
            fx,
            TimerKey::obligation(TimerType::RepeatGiveUp, obligation),
            self.ack_give_up_deadline().as_millis() as i64,
        );
    }

    /// Retain the reliable provisional as its `image()` — the datagram the
    /// transaction layer sends verbatim — and arm its first §3 rung under
    /// `AwaitPrack`. The obligation is this stack's: the `RSeq` the peer must
    /// PRACK is our own number, so the ladder under its copies is ours — per
    /// shown `(tag, rseq)`, one sequence per shown dialog (§4, errata 4603).
    /// Idempotent: a recalled number keeps the ladder it already anchors, and
    /// the give-up is armed once with the first rung so a re-emission cannot
    /// push it out.
    pub(super) fn arm_reliable_provisional_ladder(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        effect: &mut OutboundSipEffect,
        tag: &str,
        rseq: RSeq,
    ) {
        let OutboundBody::Response(resp) = &effect.body else {
            return;
        };
        let (emission, first) = RetainedEmission::paced(
            resp.image().to_vec(),
            effect.destination.clone(),
            Class::ReliableProvisional,
            Repeated::response(resp.cseq().method().as_str(), resp.status()),
        );
        if !call::helpers::start_shown_ladder(call, tag, rseq, emission) {
            return;
        }
        let obligation = Obligation::AwaitPrack { tag: tag.to_string(), rseq };
        self.schedule(
            call,
            fx,
            TimerKey::obligation(TimerType::Rung, obligation.clone()),
            first.as_millis() as i64,
        );
        self.schedule(
            call,
            fx,
            TimerKey::obligation(TimerType::RepeatGiveUp, obligation),
            Schedule::rfc(Class::ReliableProvisional).give_up_after().as_millis() as i64,
        );
    }

    // ── the framework's turns ──────────────────────────────────────────────

    /// A rung fired: re-send the retained emission `obligation` is owed for,
    /// as the bytes it left as — a rung is THE response, never a recomposed
    /// equivalent — then arm the next rung, or cease at the last one inside
    /// the bound while the give-up stands. A spent fire — the ladder is
    /// discharged, the call is no longer active, or the reliable provisional's
    /// raising fork is torn down (a reclaim can restore a ladder whose cancel
    /// died with the crashed node) — sends nothing and retires the obligation
    /// so a later reclaim cannot re-fire it. A rung that arms the next one
    /// leaves the repeat and its re-arm as the turn's whole effect set
    /// ([`QuietTurn::OwnRung`](crate::effects::QuietTurn::OwnRung)); a cease or
    /// a spent fire cancels a ledger entry, and the router reads that as the
    /// write it is.
    pub fn repeat(&self, call: &mut Call, fx: &mut HandlerEffects, obligation: &Obligation) {
        let rung = TimerKey::obligation(TimerType::Rung, obligation.clone());
        let effect = (call.state == CallModelState::Active)
            .then(|| repeat_toward(call, obligation))
            .flatten();
        let Some(effect) = effect else {
            self.retire(call, fx, Scope::Obligation(obligation.clone()));
            return;
        };
        fx.outbound.push(effect);
        let give_up = match obligation {
            Obligation::AckOf2xx { .. } => Some(self.ack_ladder_bound()),
            Obligation::AwaitPrack { .. } => None,
        };
        let (updated, next) = call::helpers::advance_ladder(call.clone(), obligation, give_up);
        *call = updated;
        match next {
            Some(next) => self.schedule(call, fx, rung, next.as_millis() as i64),
            None => {
                // The last rung inside the bound has been sent: re-asking stops
                // here, but the give-up stands — the peer's silence is answered
                // at the deadline instead of merely falling quiet. A 2xx stays
                // retained until then: it is also the RFC 6026 *Accepted*
                // marker `reinvite-glare` reads.
                self.disarm(call, fx, &rung);
                if let Obligation::AwaitPrack { tag, rseq } = obligation {
                    call::helpers::cease_shown_ladder(call, tag, *rseq, Ceased::GiveUp);
                }
            }
        }
    }

    /// The give-up fired: both ladder timers leave the ledger before the CORE
    /// give-up rule decides what the silence means. A reliable provisional's
    /// emission is spent with it; a 2xx's stays until the rules have read it
    /// ([`settle_give_up`](Self::settle_give_up)) — it is also what tells the
    /// call's own answer from a re-INVITE's.
    pub fn give_up(&self, call: &mut Call, fx: &mut HandlerEffects, obligation: &Obligation) {
        self.scrub_timers(call, fx, obligation);
        if let Obligation::AwaitPrack { tag, rseq } = obligation {
            call::helpers::cease_shown_ladder(call, tag, *rseq, Ceased::GiveUp);
        }
    }

    /// After the rules have answered the give-up. An un-ACKed 2xx's silence
    /// ends the session whatever they decided (RFC 3261 §13.3.1.4): a service
    /// re-authors the teardown — its cause, its CDR, the order the legs go —
    /// never whether it happens, so a call the rules left Active is torn down
    /// here with the CORE verdict, and the retained 2xx leaves the body either
    /// way (its RFC 6026 *Accepted* interval ended with the ladder). A reliable
    /// provisional's give-up keeps the rules' own verdict: RFC 3262 §3 rejects
    /// a transaction, and only the initial INVITE's is the call.
    pub fn settle_give_up(
        &self,
        result: HandlerResult,
        obligation: &Obligation,
        turn: &RuleContext,
    ) -> HandlerResult {
        let Obligation::AckOf2xx { .. } = obligation else {
            return result;
        };
        let HandlerResult { call, mut effects } = result;
        let forced = (call.state == CallModelState::Active)
            .then(|| unacked_2xx_give_up_actions(&RuleCall::new(&call), obligation));
        let call = call::helpers::clear_retained(call, obligation);
        let Some(actions) = forced else {
            return HandlerResult { call, effects };
        };
        let ctx = RuleContext {
            call: RuleCall::new(&call),
            call_ref: turn.call_ref,
            event: turn.event,
            source_leg_id: turn.source_leg_id,
            direction: turn.direction,
            now_ms: turn.now_ms,
            config: turn.config,
            discharged: turn.discharged,
            resolved: turn.resolved,
        };
        let forced = self.execute(&actions, &call, &ctx);
        effects.extend(forced.effects);
        HandlerResult { call: forced.call, effects }
    }

    /// The engine's match of an inbound ACK or PRACK against what the call
    /// awaits, run before the rules: an ACK whose To-tag and CSeq name a 2xx
    /// awaiting it (RFC 3261 §13.3.1.4), or a PRACK whose `RAck` names, on all
    /// three §7.2 tokens, a reliable provisional this stack showed in the
    /// dialog it arrives in and its party had not acknowledged (RFC 3262 §3).
    /// The provisional is then acknowledged and its ladder retired, repeating
    /// or not. Returns the key, for the rules to read as a fact; `None` when
    /// nothing awaited was named — a retransmitted ACK, a PRACK of an
    /// acknowledged provisional, any other request.
    pub fn discharge(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        event: &CallEvent,
        source_leg_id: &str,
    ) -> Option<Obligation> {
        let CallEvent::Sip { message, .. } = event else {
            return None;
        };
        let SipMessage::Request(req) = message.as_ref() else {
            return None;
        };
        let dialog_tag = req.to().tag().unwrap_or_default();
        let obligation = match req.method() {
            Method::Ack => {
                call::helpers::acked_2xx(call, source_leg_id, dialog_tag, req.cseq().seq() as i64)?
            }
            Method::Prack => {
                let rack = req.header::<RAck>().and_then(Result::ok)?;
                let tokens = RAckTokens {
                    rseq: RSeq(rack.rseq()),
                    cseq: CSeq(rack.seq()),
                    names_invite: *rack.method() == Method::Invite,
                };
                call::helpers::named_by(call, dialog_tag, tokens)?;
                if !call::helpers::acknowledge_shown(call, dialog_tag, tokens.rseq) {
                    return None;
                }
                Obligation::AwaitPrack { tag: dialog_tag.to_string(), rseq: tokens.rseq }
            }
            _ => return None,
        };
        self.retire(call, fx, Scope::Obligation(obligation.clone()));
        Some(obligation)
    }

    /// Remove BOTH of one obligation's timers — the rung and the give-up — so
    /// a retirement never leaves the bound running under a ladder that is
    /// gone.
    pub(super) fn scrub_timers(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        obligation: &Obligation,
    ) {
        self.scrub_where(call, fx, |t| t.key.obligation_of() == Some(obligation));
    }

    /// The bound of a §13.3.1.4 ladder — no rung is scheduled at or past it:
    /// Timer L (64·T1), where RFC 3261 ceases whatever local policy says, or
    /// the deployment's ACK deadline where that comes sooner (a rung after the
    /// teardown would repeat into a dead call). Protocol, not policy.
    fn ack_ladder_bound(&self) -> Duration {
        self.ack_give_up_deadline().min(Schedule::rfc(Class::Final2xx).give_up_after())
    }

    /// When a §13.3.1.4 give-up fires — policy, distinct from the ladder's
    /// bound: the deployment's ACK deadline, either side of Timer L
    /// (`ack_timeout_ms`, never non-positive).
    fn ack_give_up_deadline(&self) -> Duration {
        Duration::from_millis(self.config.ack_timeout_ms())
    }
}

/// The repeat `obligation`'s rung sends, toward the leg that owes the
/// discharge — `None` when nothing is left to repeat: no retained emission, or
/// a reliable provisional whose raising fork is torn down (a dead fork's
/// answer is not re-offered, and its silence must not reject a call another
/// attempt may answer).
fn repeat_toward(call: &Call, obligation: &Obligation) -> Option<OutboundSipEffect> {
    let emission = call::helpers::retained_for(call, obligation)?;
    let (label, leg) = match obligation {
        Obligation::AckOf2xx { leg, .. } => {
            let what = if call::helpers::answers_initial_invite(call, obligation) {
                "2xx"
            } else {
                "re-INVITE 2xx"
            };
            (format!("200 ({what} retransmit, no ACK) → {leg}"), leg.clone())
        }
        Obligation::AwaitPrack { tag, rseq } => {
            let raised_by_live_leg = call::helpers::responder_of(call, tag, *rseq)
                .and_then(|id| call::helpers::find_leg(call, &id.leg))
                .is_some_and(|l| l.state() != LegState::Terminated);
            if !raised_by_live_leg {
                return None;
            }
            // The rung goes to the leg the number was shown on; an a-facing
            // fork tag names no dialog of its own and is the a-leg's.
            let shown = call::helpers::leg_shown(call, tag)
                .unwrap_or(call.a_leg.leg_id.as_str())
                .to_string();
            (format!("1xx (reliable retransmit, no PRACK) → {shown}"), shown)
        }
    };
    Some(repeat_of(emission, label, &leg))
}

/// A 2xx retained on the `Final2xx` ladder with the key its ACK carries —
/// the To-tag and CSeq the response itself states — and the wait before the
/// first rung. `leg` is the face the 2xx leaves on. The retained bytes are
/// `resp.image()`: the datagram `send_response` puts on the wire, not a
/// rendering of the response (ADR-0032 X3).
fn unacked_2xx_of(
    resp: &SipResponse,
    dest: (String, u16),
    leg: &str,
) -> (Unacked2xx, Obligation, Duration) {
    let dialog_tag = resp.to().tag().unwrap_or_default().to_string();
    let cseq = resp.cseq().seq() as i64;
    let (emission, first) = RetainedEmission::paced(
        resp.image().to_vec(),
        dest,
        Class::Final2xx,
        Repeated::response(resp.cseq().method().as_str(), resp.status()),
    );
    let obligation =
        Obligation::AckOf2xx { leg: leg.to_string(), dialog_tag: dialog_tag.clone(), cseq };
    (Unacked2xx { dialog_tag, cseq, emission }, obligation, first)
}

/// The repeat of a retained emission toward `leg_id`: its bytes, to its
/// destination, past the transaction that sent the original (RFC 3261
/// §13.3.1.4 — a retransmit is THE response).
pub(super) fn repeat_of(
    emission: &RetainedEmission,
    label: String,
    leg_id: &str,
) -> OutboundSipEffect {
    let (_, (host, port)) = emission.wire();
    OutboundSipEffect {
        body: OutboundBody::Datagram(emission.clone()),
        mode: OutboundTxnMode::Raw,
        destination: (host.to_string(), port),
        label,
        leg_id: Some(leg_id.to_string()),
        provenance: Provenance::Authored,
    }
}

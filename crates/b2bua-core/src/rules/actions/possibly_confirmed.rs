//! The possibly-confirmed hold (ADR-0055 §4): a caller the record holds as
//! unanswered, on a call whose callee leg evidence proves answered, may hold
//! a 2xx another owner relayed under the tag fronting that callee dialog. She
//! is sent the owed non-2xx under that tag at once (a UAC that has the 2xx
//! discards it, RFC 6026 §8.4) and the dialog is held: BYEd on her ACK naming
//! it, else once the 2xx she may hold could no longer be awaiting its ACK
//! (64·T1, RFC 3261 §13.3.1.4, §15). A BYE that meets no dialog draws a 481.

use call::{Call, CallModelState, LegState, TimerKey, TimerType};
use sip_message::{Method, SipMessage};

use crate::effects::HandlerEffects;
use b2bua_sdk::event::CallEvent;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Hold the caller dialog fronting the callee dialog `callee_tag` of
    /// `b_leg`, which answered: the caller's INVITE takes its non-2xx under
    /// that dialog's tag now and the dialog is BYEd later. Nothing when the
    /// caller's INVITE already carries its final.
    pub(super) fn hold_possibly_confirmed(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        b_leg: &str,
        callee_tag: &str,
    ) {
        if callee_tag.is_empty()
            || call.a_leg.invite_final_sent().is_some()
            || call.a_leg.state() == LegState::Confirmed
        {
            return;
        }
        // The tag a 2xx relayed from that dialog carried.
        let tag = if call::helpers::find_leg(call, b_leg).is_some() {
            self.a_face_tag(call, b_leg, callee_tag, true)
        } else {
            self.unknown_leg_face_tag(call, b_leg, callee_tag)
        };
        crate::rules::invariants::answer_unanswered_caller(
            call,
            fx,
            self.now_ms,
            &self.config.minted_final_advertisement,
            &mut self.turn.notes,
            Some(tag.clone()),
        );
        if call.a_leg.invite_final_sent().is_none() {
            return;
        }
        let until =
            ::call::millis_i64(std::time::Duration::from_millis(self.config.ack_timeout_ms()));
        self.schedule(call, fx, hold_key(tag), until);
    }

    /// End the hold of the caller dialog `tag`: its BYE leaves, and the
    /// teardown's backstop runs again from it.
    pub fn release_possibly_confirmed(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        tag: &str,
    ) {
        self.disarm(call, fx, &hold_key(tag.to_string()));
        let Some(id) = call::helpers::caller_dialog(call, tag).map(call::Dialog::id) else {
            return;
        };
        let leg = call.a_leg.leg_id.clone();
        let bye = call::QueuedRequest::Bye {
            reason: None,
            headers: Vec::new(),
            body: Vec::new(),
            content_type: None,
            max_forwards: None,
        };
        self.send_queued_bye(call, fx, &leg, &id, bye);
    }
}

fn hold_key(tag: String) -> TimerKey {
    TimerKey::call(TimerType::PossiblyConfirmed { tag })
}

/// The caller dialog whose hold `event` ends: the fire of its hold, or her
/// ACK of the INVITE naming a held dialog by its To-tag.
pub fn released_by(call: &Call, event: &CallEvent, source_leg_id: &str) -> Option<String> {
    match event {
        CallEvent::Timer { key, .. } => match &key.timer_type {
            TimerType::PossiblyConfirmed { tag } => Some(tag.clone()),
            _ => None,
        },
        CallEvent::Sip { message, .. } => {
            let SipMessage::Request(ack) = message.as_ref() else { return None };
            let named = ack.to().tag()?;
            (*ack.method() == Method::Ack
                && source_leg_id == call.a_leg.leg_id
                && ack.cseq().seq() == call.a_leg_invite.cseq
                && holds(call, named))
            .then(|| named.to_string())
        }
        _ => None,
    }
}

/// Whether the caller dialog `tag` is held possibly confirmed.
pub fn holds(call: &Call, tag: &str) -> bool {
    call.timers
        .iter()
        .any(|t| matches!(t.timer_type(), TimerType::PossiblyConfirmed { tag: t } if t == tag))
}

/// Whether the call holds any caller dialog possibly confirmed: a
/// terminating call waits for it.
pub fn holds_any(call: &Call) -> bool {
    call.state != CallModelState::Terminated
        && call.timers.iter().any(|t| matches!(t.timer_type(), TimerType::PossiblyConfirmed { .. }))
}

/// How long after `now_ms` the last hold of the call ends, if it holds one.
pub fn held_for(call: &Call, now_ms: i64) -> Option<i64> {
    call.timers
        .iter()
        .filter(|t| matches!(t.timer_type(), TimerType::PossiblyConfirmed { .. }))
        .map(|t| t.fire_at - now_ms)
        .max()
}

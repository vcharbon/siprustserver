//! This stack's NOTIFYs and BYE behind its own NOTIFY on a dialog
//! ([`OwnNotify`]): one asked for while a NOTIFY of this stack on the same
//! dialog has no final is held as what it says, and is composed when that
//! final comes or the [`TimerType::NotifyAnswerCap`] fires — so it takes the
//! dialog's next CSeq then, and loss and retransmission never deliver this
//! stack's requests out of CSeq order (RFC 3261 §12.2.2). The record rides the
//! dialog and the cap the ledger, so a takeover copy resumes the wait and
//! composes what it releases itself.

use call::helpers::find_leg;
use call::{Call, DialogId, DialogState, HeldRequest, OwnNotify, TimerKey, TimerType};
use sip_message::{Method, SipResponse};

use crate::effects::HandlerEffects;

use super::ActionExecutor;

fn cap_key() -> TimerKey {
    TimerKey::call(TimerType::NotifyAnswerCap)
}

fn own_notify<'a>(call: &'a Call, leg_id: &str, id: &DialogId) -> Option<&'a OwnNotify> {
    find_leg(call, leg_id)?.dialogs.by_tags(&id.our_tag, &id.peer_tag)?.own_notify.as_ref()
}

fn set_own_notify(call: &mut Call, leg_id: &str, id: &DialogId, value: Option<OwnNotify>) {
    *call = call::helpers::update_dialog(call.clone(), leg_id, id, |d| d.own_notify = value);
}

/// Every dialog holding a request behind this stack's NOTIFY: its leg, its
/// tags and when its wait ends.
fn waits(call: &Call) -> Vec<(String, DialogId, i64)> {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .flat_map(|leg| leg.dialogs.iter().map(move |d| (leg, d)))
        .filter_map(|(leg, d)| {
            let n = d.own_notify.as_ref().filter(|n| !n.held.is_empty())?;
            Some((leg.leg_id.clone(), d.id(), n.cap_at_ms()))
        })
        .collect()
}

/// Whether `call` holds a request of this stack behind its own NOTIFY: a
/// terminating call is not over while one still has to leave.
pub(crate) fn holds_requests(call: &Call) -> bool {
    !waits(call).is_empty()
}

impl ActionExecutor<'_> {
    /// Whether a NOTIFY or BYE of this stack asked for now on `leg_id`'s
    /// dialog `id` waits: a NOTIFY of this stack there awaits its final, its
    /// wait not ended, or requests already wait behind it.
    pub(super) fn held_behind_notify(&mut self, call: &Call, leg_id: &str, id: &DialogId) -> bool {
        own_notify(call, leg_id, id)
            .is_some_and(|n| !n.held.is_empty() || self.now_ms < n.cap_at_ms())
    }

    /// Hold `request` behind the NOTIFY on `leg_id`'s dialog `id`.
    pub(super) fn hold(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        id: &DialogId,
        request: HeldRequest,
    ) {
        let Some(mut ahead) = own_notify(call, leg_id, id).cloned() else { return };
        ahead.held.push(request);
        set_own_notify(call, leg_id, id, Some(ahead));
        self.arm_answer_cap(call, fx);
    }

    /// The NOTIFY numbered `cseq` left on `leg_id`'s dialog `id`: what this
    /// stack asks for there next waits for its final.
    pub(super) fn notify_left(&mut self, call: &mut Call, leg_id: &str, id: &DialogId, cseq: i64) {
        let left = OwnNotify { cseq, sent_at_ms: self.now_ms, held: Vec::new() };
        set_own_notify(call, leg_id, id, Some(left));
    }

    /// `resp`, received from `leg_id`, is the final to this stack's NOTIFY
    /// awaiting one there: the requests held behind it leave.
    pub(crate) fn release_answered_notify(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        resp: &SipResponse,
    ) {
        if resp.status() < 200 || resp.cseq().method() != Method::Notify {
            return;
        }
        let cseq = i64::from(resp.cseq().seq());
        let answered = find_leg(call, leg_id).and_then(|leg| {
            leg.dialogs.iter().find(|d| d.own_notify.as_ref().is_some_and(|n| n.cseq == cseq))
        });
        if let Some(id) = answered.map(|d| d.id()) {
            self.release_held(call, fx, leg_id, &id);
            self.arm_answer_cap(call, fx);
        }
    }

    /// The [`TimerType::NotifyAnswerCap`] fired: the requests whose wait ended
    /// leave as if the NOTIFY ahead of them had been answered.
    pub(crate) fn release_at_cap(&mut self, call: &mut Call, fx: &mut HandlerEffects) {
        for (leg_id, id, cap_at) in waits(call) {
            if cap_at <= self.now_ms {
                self.release_held(call, fx, &leg_id, &id);
            }
        }
        self.arm_answer_cap(call, fx);
    }

    /// The cap stands at the earliest end of a wait, and is off while nothing
    /// is held.
    fn arm_answer_cap(&mut self, call: &mut Call, fx: &mut HandlerEffects) {
        match waits(call).into_iter().map(|(_, _, cap_at)| cap_at).min() {
            Some(cap_at) => self.schedule(call, fx, cap_key(), (cap_at - self.now_ms).max(0)),
            None => self.disarm(call, fx, &cap_key()),
        }
    }

    /// The NOTIFY ahead on `leg_id`'s dialog `id` is done: the held requests
    /// leave in order up to and including the next NOTIFY, which awaits its
    /// final with the rest held behind it. A BYE held on a dialog that ended is
    /// dropped; a NOTIFY still leaves, its subscription outliving the dialog's
    /// INVITE usage (RFC 5057 §5.2).
    fn release_held(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        id: &DialogId,
    ) {
        let Some(done) = own_notify(call, leg_id, id).cloned() else { return };
        set_own_notify(call, leg_id, id, None);
        let ended = find_leg(call, leg_id)
            .and_then(|leg| leg.dialogs.by_tags(&id.our_tag, &id.peer_tag))
            .is_some_and(|d| matches!(d.state, DialogState::Ended(_)));
        let mut held = done.held.into_iter();
        while let Some(request) = held.next() {
            match request {
                HeldRequest::Notify { event, subscription_state, content_type, body } => {
                    let content_type = content_type.as_deref();
                    self.notify_now(
                        call,
                        fx,
                        leg_id,
                        id,
                        &event,
                        &subscription_state,
                        content_type,
                        &body,
                    );
                    if let Some(mut next) = own_notify(call, leg_id, id).cloned() {
                        next.held = held.collect();
                        set_own_notify(call, leg_id, id, Some(next));
                    }
                    return;
                }
                HeldRequest::Bye { .. } if ended => {}
                bye @ HeldRequest::Bye { .. } => self.send_held_bye(call, fx, leg_id, id, bye),
            }
        }
    }
}

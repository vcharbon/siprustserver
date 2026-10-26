//! This stack as the notifier of a dialog's subscriptions ([`Subscription`],
//! ADR-0055 §2), and the order of its requests behind its own NOTIFY there.
//!
//! - A NOTIFY on a subscription that is terminated is refused: once one
//!   stating `terminated` was asked for, or a final or timeout ended it, no
//!   NOTIFY leaves on it (RFC 6665 §4.2.2), whoever asks.
//! - A NOTIFY or BYE asked for while a NOTIFY of this stack on the same dialog
//!   awaits its final is queued as what it says, and composed when that final
//!   comes or the [`TimerType::NotifyAnswerCap`] fires, at most 8·T1 after the
//!   NOTIFY left: it takes the dialog's next CSeq then, so loss and
//!   retransmission never deliver this stack's requests out of CSeq order
//!   (RFC 3261 §12.2.2).
//! - A final that removes the subscription, or the NOTIFY's timeout, ends it
//!   and drops its queued NOTIFYs; the rest of the queue still leaves.
//!
//! The record rides the dialog and the cap the ledger, so a restored copy of
//! the call resumes the wait and composes what it releases itself.

use call::helpers::{find_leg, RecordHolder};
use call::{
    Call, DialogId, DialogState, DialogUsage, NotifyInFlight, QueuedRequest, RequestRecord,
    Subscription, SubscriptionState, TimerKey, TimerType,
};
use sip_message::header::{Event, HeaderValue, SubscriptionState as SubscriptionStateLine};
use sip_message::{Method, SipStr};

use crate::effects::HandlerEffects;
use crate::rules::method_table::{row, subscription_key};

use super::select::leg_at;
use super::ActionExecutor;

/// The finals to a NOTIFY that remove its subscription (RFC 6665 §4.2.2).
const SUBSCRIPTION_ENDING: [u16; 13] =
    [404, 405, 410, 416, 480, 481, 482, 483, 484, 485, 489, 501, 604];

fn cap_key() -> TimerKey {
    TimerKey::call(TimerType::NotifyAnswerCap)
}

/// The subscription an `Event` value names: its package and `id`. A value
/// the reader rejects names the package it spells.
fn named(event: &str) -> (String, Option<String>) {
    match Event::parse(&SipStr::owned(event)) {
        Ok(event) => subscription_key(&event),
        Err(_) => (event.trim().to_ascii_lowercase(), None),
    }
}

/// The state a `Subscription-State` value reports (RFC 6665 §8.2.3).
fn reported(subscription_state: &str) -> SubscriptionState {
    let token = SubscriptionStateLine::parse(&SipStr::owned(subscription_state))
        .map(|s| s.token().to_ascii_lowercase())
        .unwrap_or_else(|_| subscription_state.trim().to_ascii_lowercase());
    match token.as_str() {
        "terminated" => SubscriptionState::Terminated,
        "active" => SubscriptionState::Active,
        _ => SubscriptionState::Pending,
    }
}

fn dialog_of<'a>(call: &'a Call, leg_id: &str, id: &DialogId) -> Option<&'a call::Dialog> {
    find_leg(call, leg_id)?.dialogs.by_tags(&id.our_tag, &id.peer_tag)
}

/// Every dialog with requests queued: its leg, its tags and when its wait
/// ends (`None`: nothing is in flight ahead of them).
fn waits(call: &Call) -> Vec<(String, DialogId, Option<i64>)> {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .flat_map(|leg| leg.dialogs.iter().map(move |d| (leg, d)))
        .filter(|(_, d)| !d.queued.is_empty())
        .map(|(leg, d)| (leg.leg_id.clone(), d.id(), d.notify_wait_ends_at_ms()))
        .collect()
}

/// Whether `call` holds a request of this stack queued behind its own
/// NOTIFY: a terminating call is not over while one still has to leave.
pub(crate) fn holds_requests(call: &Call) -> bool {
    !waits(call).is_empty()
}

impl ActionExecutor<'_> {
    /// Ask for a NOTIFY on `leg_id`'s carrying dialog, on the subscription
    /// `event` names, reporting `subscription_state` with `body`: refused on a
    /// terminated subscription, queued behind this stack's NOTIFY in flight
    /// there, sent otherwise. The subscription opens with its first NOTIFY
    /// and records the state each one reports as it is asked for.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send_notify(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        event: &str,
        subscription_state: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) {
        let Some(id) =
            leg_at(call, leg_id).and_then(call::helpers::carrying_dialog).map(|d| d.id())
        else {
            return;
        };
        let (package, sub_id) = named(event);
        let over = dialog_of(call, leg_id, &id)
            .and_then(|d| d.subscription(&package, sub_id.as_deref()))
            .is_some_and(Subscription::is_terminated);
        if over {
            tracing::debug!(
                call_ref = %call.call_ref,
                %leg_id,
                event,
                "NOTIFY refused: its subscription is terminated (RFC 6665 §4.2.2)"
            );
            return;
        }
        let state = reported(subscription_state);
        *call = call::helpers::update_dialog(call.clone(), leg_id, &id, |d| {
            match d.subscriptions.iter_mut().find(|s| s.is_named(&package, sub_id.as_deref())) {
                Some(s) => s.report(state),
                None => {
                    let mut opened = Subscription::new(&package, sub_id.as_deref());
                    opened.report(state);
                    d.subscriptions.push(opened);
                }
            }
        });
        if row(&Method::Notify).waits_behind_own_notify
            && self.waits_behind_notify(call, leg_id, &id)
        {
            let notify = QueuedRequest::Notify {
                event: event.to_string(),
                subscription_state: subscription_state.to_string(),
                content_type: content_type.map(str::to_string),
                body: body.to_vec(),
            };
            self.queue(call, fx, leg_id, &id, notify);
            return;
        }
        self.notify_now(call, fx, leg_id, &id, event, subscription_state, content_type, body);
    }

    /// Whether a NOTIFY or BYE of this stack asked for now on `leg_id`'s
    /// dialog `id` waits: requests are already queued there, or a NOTIFY of
    /// this stack there awaits its final, its wait not ended.
    pub(super) fn waits_behind_notify(&mut self, call: &Call, leg_id: &str, id: &DialogId) -> bool {
        dialog_of(call, leg_id, id).is_some_and(|d| {
            !d.queued.is_empty() || d.notify_wait_ends_at_ms().is_some_and(|end| self.now_ms < end)
        })
    }

    /// Queue `request` behind the NOTIFY on `leg_id`'s dialog `id`.
    pub(super) fn queue(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        id: &DialogId,
        request: QueuedRequest,
    ) {
        *call = call::helpers::update_dialog(call.clone(), leg_id, id, |d| d.queued.push(request));
        self.arm_answer_cap(call, fx);
    }

    /// The NOTIFY numbered `cseq` on the subscription `event` names left on
    /// `leg_id`'s dialog `id`: what this stack asks for there next waits for
    /// its final.
    pub(super) fn notify_left(
        &mut self,
        call: &mut Call,
        leg_id: &str,
        id: &DialogId,
        event: &str,
        cseq: i64,
    ) {
        let (package, sub_id) = named(event);
        let left = NotifyInFlight { cseq, sent_at_ms: self.now_ms };
        *call = call::helpers::update_dialog(call.clone(), leg_id, id, |d| {
            if let Some(s) =
                d.subscriptions.iter_mut().find(|s| s.is_named(&package, sub_id.as_deref()))
            {
                s.notify = Some(left);
            }
        });
    }

    /// This stack's NOTIFY `record`, held on `leg_id` by `holder`, drew the
    /// final `status`, or timed out (`None`): it no longer holds the queue
    /// behind it, and a final that removes the subscription or the timeout
    /// ends that subscription and drops its queued NOTIFYs (RFC 6665
    /// §4.2.2). What the queue holds next leaves.
    pub(crate) fn notify_settled(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        holder: &RecordHolder,
        record: &RequestRecord,
        status: Option<u16>,
    ) {
        let (RecordHolder::Dialog(id), DialogUsage::Subscription { event, id: sub_id }) =
            (holder, &record.usage)
        else {
            return;
        };
        if !record.is_own() || record.method != "NOTIFY" {
            return;
        }
        let ends = status.is_none_or(|s| SUBSCRIPTION_ENDING.contains(&s));
        let cseq = record.outbound_cseq;
        *call = call::helpers::update_dialog(call.clone(), leg_id, id, |d| {
            let Some(s) = d.subscriptions.iter_mut().find(|s| s.is_named(event, sub_id.as_deref()))
            else {
                return;
            };
            if s.notify.is_some_and(|n| n.cseq == cseq) {
                s.notify = None;
            }
            if ends {
                s.state = SubscriptionState::Terminated;
                d.queued.retain(|q| match q {
                    QueuedRequest::Notify { event: e, .. } => {
                        let (package, named_id) = named(e);
                        !(package == *event && named_id == *sub_id)
                    }
                    QueuedRequest::Bye { .. } => true,
                });
            }
        });
        self.release_queued(call, fx, leg_id, id);
        self.arm_answer_cap(call, fx);
    }

    /// The [`TimerType::NotifyAnswerCap`] fired: the requests whose wait ended
    /// leave as if the NOTIFY ahead of them had been answered.
    pub(crate) fn release_at_cap(&mut self, call: &mut Call, fx: &mut HandlerEffects) {
        for (leg_id, id, ends_at) in waits(call) {
            if ends_at.is_none_or(|end| end <= self.now_ms) {
                self.release_queued(call, fx, &leg_id, &id);
            }
        }
        self.arm_answer_cap(call, fx);
    }

    /// The cap stands at the earliest end of a wait, and is off while nothing
    /// is queued.
    fn arm_answer_cap(&mut self, call: &mut Call, fx: &mut HandlerEffects) {
        let ends = waits(call).into_iter().map(|(_, _, end)| end.unwrap_or(self.now_ms));
        match ends.min() {
            Some(end) => self.schedule(call, fx, cap_key(), (end - self.now_ms).max(0)),
            None => self.disarm(call, fx, &cap_key()),
        }
    }

    /// The queue on `leg_id`'s dialog `id` leaves in order while nothing of
    /// this stack is in flight ahead of it: up to and including the next
    /// NOTIFY, which then awaits its final with the rest behind it. A BYE
    /// queued on a dialog that ended is dropped; a NOTIFY still leaves, its
    /// subscription outliving the dialog's INVITE usage (RFC 5057 §5.2).
    fn release_queued(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        id: &DialogId,
    ) {
        loop {
            let Some(d) = dialog_of(call, leg_id, id) else { return };
            let in_flight = d.notify_wait_ends_at_ms().is_some_and(|end| self.now_ms < end);
            if d.queued.is_empty() || in_flight {
                return;
            }
            let ended = matches!(d.state, DialogState::Ended(_));
            let mut next = None;
            *call = call::helpers::update_dialog(call.clone(), leg_id, id, |d| {
                let mut queued = std::mem::take(&mut d.queued).into_iter();
                next = queued.next();
                d.queued = queued.collect();
            });
            match next {
                Some(QueuedRequest::Notify { event, subscription_state, content_type, body }) => {
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
                }
                Some(QueuedRequest::Bye { .. }) if ended => {}
                Some(bye @ QueuedRequest::Bye { .. }) => {
                    self.send_queued_bye(call, fx, leg_id, id, bye);
                }
                None => return,
            }
        }
    }
}

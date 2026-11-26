//! What the transaction layer holds for a turn until the turn's outbox entry
//! releases it: the ACK of a non-2xx final the turn took, and the 200 to a
//! CANCEL with the 487 to its INVITE.

use b2bua_sdk::event::CallEvent;
use call::Call;
use sip_message::{Method, SipMessage, SipRequest};
use sip_txn::CancelledInvite;

use crate::router::RouterCtx;

/// The CANCEL of the INVITE named by its Call-ID, From-tag and CSeq number,
/// whose answers the layer holds, and whether that INVITE is a re-INVITE.
pub struct HeldCancel {
    pub call_id: String,
    pub from_tag: String,
    pub cseq: u32,
    pub in_dialog: bool,
}

impl HeldCancel {
    /// The CANCEL of `invite`, the caller's initial INVITE.
    pub fn of(invite: &SipRequest) -> Self {
        Self {
            call_id: invite.call_id().as_str().to_string(),
            from_tag: invite.from().tag().unwrap_or_default().to_string(),
            cseq: invite.cseq().seq(),
            in_dialog: false,
        }
    }

    /// Let the layer send the 200 to the CANCEL and the 487 to its INVITE,
    /// as `call`, the record the turn landed, has it: a re-INVITE whose
    /// CANCEL the turn relayed toward its target stays open for the target's
    /// final ([`call::helpers::holds_relayed_cancel`], ADR-0065).
    pub async fn release(&self, ctx: &RouterCtx, call: &Call) {
        let relayed = self.in_dialog
            && call::helpers::holds_relayed_cancel(call, &self.call_id, i64::from(self.cseq));
        let invite = if relayed { CancelledInvite::Keep } else { CancelledInvite::End };
        let _ = ctx.txn.release_cancel(&self.call_id, &self.from_tag, self.cseq, invite).await;
    }
}

/// What the layer holds for the turn `trigger` starts: the branch of the
/// client INVITE whose non-2xx final it is, and the CANCEL it reports.
pub fn of(trigger: &CallEvent) -> (Option<String>, Option<HeldCancel>) {
    let ack = match trigger {
        CallEvent::Sip { message, .. } => held_ack_branch(message),
        _ => None,
    };
    (ack, held_cancel_answers(trigger))
}

/// The branch of the client INVITE transaction whose non-2xx final `trigger`
/// is: the layer holds its ACK until the turn's entry releases it.
fn held_ack_branch(trigger: &SipMessage) -> Option<String> {
    let SipMessage::Response(resp) = trigger else { return None };
    if *resp.cseq().method() != Method::Invite || resp.status() < 300 {
        return None;
    }
    resp.top_via().branch().filter(|b| !b.is_empty()).map(str::to_string)
}

/// The CANCEL `trigger` reports: the layer holds the 200 to it and the 487
/// to the INVITE until the turn's entry releases them.
fn held_cancel_answers(trigger: &CallEvent) -> Option<HeldCancel> {
    let CallEvent::Cancelled { call_id, from_tag, invite_cseq, in_dialog, .. } = trigger else {
        return None;
    };
    let (call_id, from_tag) = (call_id.clone(), from_tag.clone());
    Some(HeldCancel { call_id, from_tag, cseq: (*invite_cseq)?, in_dialog: *in_dialog })
}

/// Release what the layer holds for `event`'s turn when no turn's outbox
/// entry took it: no write records it, so nothing is waited for, and a
/// cancelled INVITE ends with the layer's 487.
pub async fn release_untaken(ctx: &RouterCtx, event: &CallEvent) {
    let ack = match event {
        CallEvent::Sip { message, .. } => held_ack_branch(message),
        _ => None,
    };
    if let Some(branch) = ack {
        let _ = ctx.txn.release_ack(&branch).await;
    }
    if let Some(held) = held_cancel_answers(event) {
        let (call_id, from_tag, cseq) = (&held.call_id, &held.from_tag, held.cseq);
        let _ = ctx.txn.release_cancel(call_id, from_tag, cseq, CancelledInvite::End).await;
    }
}

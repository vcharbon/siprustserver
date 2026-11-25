//! The answers of an ended call stay answerable (RFC 3261 §17.2.2, Timer J):
//! each endpoint keeps the finals its agent sent and hands them to the socket
//! registry when it drops; a retransmission of an answered request arriving
//! after the call has gone draws the same final from the mux instead of being
//! a stray. Only a retransmitted request triggers the repair: a final whose
//! request the peer no longer retransmits (an INVITE past its first
//! provisional, in Proceeding) is not repaired when lost.
//!
//! Held: every final to a non-INVITE request, and an INVITE's non-2xx final
//! (whose ACK the hold then absorbs). An INVITE's 2xx is the dialog's own
//! (§13.3.1.4) and is not held: a marker without bytes, keyed by its CSeq,
//! stands for it, and an ACK of that CSeq is absorbed (the 2xx's ACK is a
//! transaction of its own, under a new branch: §13.2.2.4, §17.1.1.3). For an
//! in-dialog request that reached the endpoint, delivered or dropped by the
//! loss model on its way in, the 481 of a UA that no longer holds the dialog
//! (§12.2.2) is held until the agent answers it: a retransmission arriving
//! once the call released without answering it (the agent ended first, or
//! never got a copy) draws the 481, not a stray. A request first arriving
//! after the release stays a stray. Once the ACK of a held INVITE final
//! arrives, before or after the call released, the final is owed no more
//! (§17.2.1 Confirmed): its bytes go, and a retransmitted INVITE or ACK of
//! that transaction is absorbed.
//!
//! A CANCEL a released dialog holds no answer to is answered as a UA answers
//! it, from what the hold has (RFC 3261 §9.2): its INVITE transaction's
//! non-2xx final still held, a 200 (the CANCEL has no effect, the To tag the
//! final's); that INVITE reached the endpoint unanswered (an in-dialog
//! re-INVITE), a 200 and the INVITE's 487, then held as its final. Any other
//! CANCEL, on a released Call-ID or one never seen, draws a 481 built from
//! itself alone ([`refuse_cancel`]): the mux holds nothing more for it, and
//! an initial INVITE answered only with provisionals is not held.
//!
//! Bounds: per dialog the last [`ANSWERS_HELD`] answers and markers, each
//! answer the response datagram as sent (the copy the endpoint's other stores
//! share) with a hashed transaction key and its request's ladder arrow;
//! dropped with the endpoint, or for a released dialog when its hold
//! ([`RELEASE_HOLD`](super::RELEASE_HOLD), 64·T1 = 32 s) expires. In
//! aggregate the mux holds the calls released in the last 32 s (cps × 32), a
//! call typically 1 to 3 answers (the BYE's 200; a CANCEL's 200 and the 487)
//! of about 0.5 KiB plus 0.1 KiB of key and arrow, so 1 to 2 KiB a call:
//! about 13 MiB at 200 cps, 130 MiB at 2000 cps; at worst 32 × 64 KiB a
//! dialog.

use std::sync::{Arc, Mutex};

use sip_message::sniff::{response_to, to_tag, Identity};

use super::datagram::{key_of, Datagram};
use super::tally::LateRepair;
use super::taps::Taps;

/// The answers one dialog holds at most: the last ones it sent. Room for a
/// long reliable-provisional train's PRACKs (RFC 3262 §3, each its own
/// transaction a peer retransmits until Timer F) beside the dialog's other
/// answers.
pub(in crate::mux) const ANSWERS_HELD: usize = 32;

/// One final our agent sent, keyed by the server transaction it ends.
struct Answer {
    /// The dialog's Call-ID, hashed.
    dialog: u64,
    /// The transaction: the request's CSeq number, CSeq method and top Via
    /// branch (echoed on the response), hashed.
    txn: u64,
    /// The final as sent; `None` once its ACK arrived (an INVITE's), or
    /// while the agent has not answered (`unanswered`).
    bytes: Option<Arc<[u8]>>,
    /// The request reached the endpoint and the agent has not answered it.
    unanswered: bool,
    /// Where a late re-send of it is reported; `None`: not reported.
    repair: Option<LateRepair>,
}

/// The transaction key of a request, or of the response that answers it.
fn txn_key(cseq: u32, method: &str, branch: &str) -> u64 {
    key_of((cseq, method, branch))
}

/// The key of the marker an INVITE 2xx with CSeq number `cseq` leaves: its
/// ACK carries that number under a branch of its own (RFC 3261 §13.2.2.4).
fn answered_invite_key(cseq: u32) -> u64 {
    key_of((cseq, "INVITE 2xx"))
}

/// What a request arriving on a released dialog draws from its held answers.
pub(in crate::mux) enum LateReply {
    /// No held answer is this request's: it is a stray.
    NotHeld,
    /// The ACK of a held INVITE non-2xx final or of an INVITE 2xx the agent
    /// sent, or a retransmission of an INVITE whose final was ACKed: absorbed.
    Absorbed,
    /// A retransmission of an answered request: the same final again, and
    /// where that re-send is reported.
    Reanswer(Arc<[u8]>, Option<LateRepair>),
    /// A retransmission of a request the agent never answered: the 481 of
    /// a UA that no longer holds the dialog (RFC 3261 §12.2.2), built from
    /// the retransmission itself.
    Refuse,
    /// A CANCEL of a held INVITE transaction: the responses it draws, its
    /// 200 first.
    Cancel(Vec<Arc<[u8]>>),
}

/// The answers one released dialog holds.
pub(in crate::mux) struct HeldAnswers(Vec<Answer>);

impl HeldAnswers {
    /// What the request `d` draws from these answers; an ACK of a held
    /// final marks it ACKed, and a CANCEL of an unanswered INVITE holds the
    /// 487 it draws.
    pub(in crate::mux) fn on_request(&mut self, d: &Datagram<'_>) -> LateReply {
        let id = d.id();
        let (Some(method), Some(cseq), Some(branch)) = (id.method, id.cseq, id.branch) else {
            return LateReply::NotHeld;
        };
        if method == "ACK" {
            let held = acked(&mut self.0, cseq, branch)
                || self.0.iter().any(|a| a.txn == answered_invite_key(cseq));
            return if held { LateReply::Absorbed } else { LateReply::NotHeld };
        }
        let txn = txn_key(cseq, method, branch);
        if method == "CANCEL" {
            return match self.0.iter().find(|a| a.txn == txn && a.bytes.is_some()) {
                Some(Answer { bytes: Some(bytes), repair, .. }) => {
                    LateReply::Reanswer(bytes.clone(), repair.clone())
                }
                _ => self.on_cancel(d, txn_key(cseq, "INVITE", branch)),
            };
        }
        match self.0.iter().find(|a| a.txn == txn) {
            Some(Answer { bytes: Some(bytes), repair, .. }) => {
                LateReply::Reanswer(bytes.clone(), repair.clone())
            }
            Some(Answer { unanswered: true, .. }) => LateReply::Refuse,
            Some(_) => LateReply::Absorbed,
            None => LateReply::NotHeld,
        }
    }

    /// What the CANCEL `d` draws from the INVITE transaction `invite` it
    /// names (RFC 3261 §9.2): a 200 when its non-2xx final is held, a 200
    /// and a 487 (held from now on as that INVITE's final) when it reached
    /// the endpoint unanswered, nothing held otherwise.
    fn on_cancel(&mut self, d: &Datagram<'_>, invite: u64) -> LateReply {
        let Some(held) = self.0.iter_mut().find(|a| a.txn == invite) else {
            return LateReply::NotHeld;
        };
        if held.unanswered {
            let tag = stateless_tag(d);
            let terminated = response_to(d.raw(), 487, "Request Terminated", &tag, Some("INVITE"));
            let (Some(ok), Some(terminated)) =
                (response_to(d.raw(), 200, "OK", &tag, None), terminated)
            else {
                return LateReply::NotHeld;
            };
            let terminated: Arc<[u8]> = terminated.into();
            held.bytes = Some(terminated.clone());
            held.unanswered = false;
            return LateReply::Cancel(vec![ok.into(), terminated]);
        }
        let Some(tag) = held.bytes.as_deref().map(to_tag) else {
            return LateReply::NotHeld;
        };
        match response_to(d.raw(), 200, "OK", &tag, None) {
            Some(ok) => LateReply::Cancel(vec![ok.into()]),
            None => LateReply::NotHeld,
        }
    }
}

/// The 481 a CANCEL no held INVITE transaction answers draws (RFC 3261
/// §9.2), built from the CANCEL alone; `None` for a datagram that is no
/// CANCEL or lacks a row a response copies.
pub(in crate::mux) fn refuse_cancel(d: &Datagram<'_>) -> Option<Arc<[u8]>> {
    if d.id().method != Some("CANCEL") {
        return None;
    }
    let refused =
        response_to(d.raw(), 481, "Call/Transaction Does Not Exist", &stateless_tag(d), None);
    refused.map(Into::into)
}

/// The To tag a response built with no dialog state gives a request whose
/// To names none (RFC 3261 §8.2.6.2): derived from the request's Call-ID and
/// top Via branch, so every retransmission of it draws the same tag.
fn stateless_tag(d: &Datagram<'_>) -> String {
    format!("{:016x}", key_of((d.id().call_id, d.id().branch, "stateless")))
}

/// Mark ACKed the held INVITE final of transaction (`cseq`, `branch`) in
/// `held`: whether one is held.
fn acked(held: &mut [Answer], cseq: u32, branch: &str) -> bool {
    let txn = txn_key(cseq, "INVITE", branch);
    match held.iter_mut().find(|a| a.txn == txn) {
        Some(a) => {
            a.bytes = None;
            true
        }
        None => false,
    }
}

/// Append `answer` to `held`, first dropping its dialog's oldest answer when
/// the dialog already holds [`ANSWERS_HELD`]. An endpoint holds a handful of
/// answers: room is made for one more at a time.
fn hold(held: &mut Vec<Answer>, answer: Answer) {
    let dialog = answer.dialog;
    if held.iter().filter(|a| a.dialog == dialog).count() == ANSWERS_HELD {
        let oldest = held.iter().position(|a| a.dialog == dialog).expect("a held answer");
        held.remove(oldest);
    }
    held.reserve_exact(1);
    held.push(answer);
}

/// An endpoint's answers, every dialog's, in the order sent.
#[derive(Default)]
pub(in crate::mux) struct SentAnswers(Mutex<Vec<Answer>>);

impl SentAnswers {
    /// Hold `d` when it is a final this endpoint's agent sends that a
    /// retransmitted request may draw again; it replaces an earlier answer to
    /// the same transaction. An INVITE 2xx leaves only its marker, once per
    /// CSeq. `taps` resolve the request's ladder arrow.
    pub(in crate::mux) fn on_outbound(&self, d: &Datagram<'_>, taps: Option<&Taps>) {
        let id = d.id();
        let (Some(status), Some(cid), Some(cseq), Some(method), Some(branch)) =
            (d.final_status(), id.call_id, id.cseq, id.cseq_method, id.branch)
        else {
            return;
        };
        let dialog = key_of(cid);
        if method == "INVITE" && status < 300 {
            self.mark_answered_invite(dialog, cseq);
            return;
        }
        let repair = taps.and_then(|t| t.late_repair(id));
        let txn = txn_key(cseq, method, branch);
        let mut held = self.0.lock().unwrap();
        held.retain(|a| a.dialog != dialog || a.txn != txn);
        let answer = Answer { dialog, txn, bytes: Some(d.shared()), unanswered: false, repair };
        hold(&mut held, answer);
    }

    /// Hold the marker of an INVITE 2xx with CSeq number `cseq` on `dialog`,
    /// unless it is held already (a retransmitted 2xx).
    fn mark_answered_invite(&self, dialog: u64, cseq: u32) {
        let txn = answered_invite_key(cseq);
        let mut held = self.0.lock().unwrap();
        if !held.iter().any(|a| a.dialog == dialog && a.txn == txn) {
            let marker = Answer { dialog, txn, bytes: None, unanswered: false, repair: None };
            hold(&mut held, marker);
        }
    }

    /// Note `d`, an in-dialog request that reached this endpoint (delivered,
    /// or dropped by the loss model on its way in), as unanswered until the
    /// agent answers it.
    pub(in crate::mux) fn on_request_in(&self, d: &Datagram<'_>) {
        let id = d.id();
        let (None, Some(cid), Some(cseq), Some(method), Some(branch), Some(_)) =
            (id.status, id.call_id, id.cseq, id.method, id.branch, id.to_tag)
        else {
            return;
        };
        if method == "ACK" {
            return;
        }
        let (dialog, txn) = (key_of(cid), txn_key(cseq, method, branch));
        let mut held = self.0.lock().unwrap();
        if !held.iter().any(|a| a.dialog == dialog && a.txn == txn) {
            let answer = Answer { dialog, txn, bytes: None, unanswered: true, repair: None };
            hold(&mut held, answer);
        }
    }

    /// An ACK `id` delivered to this endpoint: the INVITE final it ACKs, if
    /// held, is owed no more.
    pub(in crate::mux) fn on_ack(&self, id: &Identity<'_>) {
        let (Some(cid), Some(cseq), Some(branch)) = (id.call_id, id.cseq, id.branch) else {
            return;
        };
        let dialog = key_of(cid);
        let mut held = self.0.lock().unwrap();
        let txn = txn_key(cseq, "INVITE", branch);
        if let Some(a) = held.iter_mut().find(|a| a.dialog == dialog && a.txn == txn) {
            a.bytes = None;
        }
    }

    /// The answers of dialog `call_id`, removed.
    pub(in crate::mux) fn take(&self, call_id: &str) -> Option<HeldAnswers> {
        let dialog = key_of(call_id);
        let mut held = self.0.lock().unwrap();
        let taken: Vec<Answer> = held.extract_if(.., |a| a.dialog == dialog).collect();
        (!taken.is_empty()).then_some(HeldAnswers(taken))
    }
}

#[cfg(test)]
#[path = "late_answer_tests.rs"]
mod tests;

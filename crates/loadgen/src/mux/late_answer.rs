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
//! (§13.3.1.4) and is not held. Once the ACK of a held INVITE final arrives,
//! before or after the call released, the final is owed no more (§17.2.1
//! Confirmed): its bytes go, and a retransmitted INVITE or ACK of that
//! transaction is absorbed.
//!
//! Bounds: per dialog the last [`ANSWERS_HELD`] answers, each the response
//! datagram as sent (the copy the endpoint's other stores share) with a
//! hashed transaction key and its request's ladder arrow; dropped with the
//! endpoint, or for a released dialog when its hold
//! ([`RELEASE_HOLD`](super::RELEASE_HOLD), 64·T1 = 32 s) expires. In
//! aggregate the mux holds the calls released in the last 32 s (cps × 32), a
//! call typically 1 to 3 answers (the BYE's 200; a CANCEL's 200 and the 487)
//! of about 0.5 KiB plus 0.1 KiB of key and arrow, so 1 to 2 KiB a call:
//! about 13 MiB at 200 cps, 130 MiB at 2000 cps; at worst 8 × 64 KiB a dialog.

use std::sync::{Arc, Mutex};

use sip_message::sniff::Identity;

use super::datagram::{key_of, Datagram};
use super::tally::LateRepair;
use super::taps::Taps;

/// The answers one dialog holds at most: the last ones it sent.
pub(in crate::mux) const ANSWERS_HELD: usize = 8;

/// One final our agent sent, keyed by the server transaction it ends.
struct Answer {
    /// The dialog's Call-ID, hashed.
    dialog: u64,
    /// The transaction: the request's CSeq number, CSeq method and top Via
    /// branch (echoed on the response), hashed.
    txn: u64,
    /// The final as sent; `None` once its ACK arrived (an INVITE's).
    bytes: Option<Arc<[u8]>>,
    /// Where a late re-send of it is reported; `None`: not reported.
    repair: Option<LateRepair>,
}

/// The transaction key of a request, or of the response that answers it.
fn txn_key(cseq: u32, method: &str, branch: &str) -> u64 {
    key_of((cseq, method, branch))
}

/// What a request arriving on a released dialog draws from its held answers.
pub(in crate::mux) enum LateReply {
    /// No held answer is this request's: it is a stray.
    NotHeld,
    /// The ACK of a held INVITE non-2xx final, or a retransmission of an
    /// INVITE whose final was ACKed: absorbed.
    Absorbed,
    /// A retransmission of an answered request: the same final again, and
    /// where that re-send is reported.
    Reanswer(Arc<[u8]>, Option<LateRepair>),
}

/// The answers one released dialog holds.
pub(in crate::mux) struct HeldAnswers(Vec<Answer>);

impl HeldAnswers {
    /// What the request `id` draws from these answers; an ACK of a held
    /// final marks it ACKed.
    pub(in crate::mux) fn on_request(&mut self, id: &Identity<'_>) -> LateReply {
        let (Some(method), Some(cseq), Some(branch)) = (id.method, id.cseq, id.branch) else {
            return LateReply::NotHeld;
        };
        if method == "ACK" {
            let held = acked(&mut self.0, cseq, branch);
            return if held { LateReply::Absorbed } else { LateReply::NotHeld };
        }
        let txn = txn_key(cseq, method, branch);
        match self.0.iter().find(|a| a.txn == txn) {
            Some(Answer { bytes: Some(bytes), repair, .. }) => {
                LateReply::Reanswer(bytes.clone(), repair.clone())
            }
            Some(_) => LateReply::Absorbed,
            None => LateReply::NotHeld,
        }
    }
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

/// An endpoint's answers, every dialog's, in the order sent.
#[derive(Default)]
pub(in crate::mux) struct SentAnswers(Mutex<Vec<Answer>>);

impl SentAnswers {
    /// Hold `d` when it is a final this endpoint's agent sends that a
    /// retransmitted request may draw again; it replaces an earlier answer to
    /// the same transaction. `taps` resolve the request's ladder arrow.
    pub(in crate::mux) fn on_outbound(&self, d: &Datagram<'_>, taps: Option<&Taps>) {
        let id = d.id();
        let (Some(status), Some(cid), Some(cseq), Some(method), Some(branch)) =
            (d.final_status(), id.call_id, id.cseq, id.cseq_method, id.branch)
        else {
            return;
        };
        if method == "INVITE" && status < 300 {
            return;
        }
        let repair = taps.and_then(|t| t.late_repair(id));
        let (dialog, txn) = (key_of(cid), txn_key(cseq, method, branch));
        let mut held = self.0.lock().unwrap();
        held.retain(|a| a.dialog != dialog || a.txn != txn);
        if held.iter().filter(|a| a.dialog == dialog).count() == ANSWERS_HELD {
            let oldest = held.iter().position(|a| a.dialog == dialog).expect("a held answer");
            held.remove(oldest);
        }
        // An endpoint holds a handful of answers: room for one more at a time.
        held.reserve_exact(1);
        held.push(Answer { dialog, txn, bytes: Some(d.shared()), repair });
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

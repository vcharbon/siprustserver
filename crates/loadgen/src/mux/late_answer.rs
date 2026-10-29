//! A server transaction outlives the call that answered it (RFC 3261 §17.2.2
//! Timer J, §17.2.1 for an INVITE's non-2xx final): each endpoint keeps the
//! finals its agent sent, hands them to the socket registry when it drops, and
//! a retransmission of an answered request arriving after the call has gone
//! draws the same final from the mux instead of being a stray.
//!
//! Held: every final to a non-INVITE request, and an INVITE's non-2xx final
//! (whose ACK the hold then absorbs). An INVITE's 2xx is the dialog's own
//! (§13.3.1.4) and is not held.
//!
//! Bounds: per dialog the last [`ANSWERS_HELD`] answers, each the response
//! datagram as sent (one UDP datagram, a few hundred bytes typically, under
//! 64 KiB) with its transaction key and its request's ladder arrow; dropped
//! with the endpoint, or for a released dialog when its hold
//! ([`RELEASE_HOLD`](super::RELEASE_HOLD), 64·T1) expires.

use std::collections::HashMap;
use std::sync::Mutex;

use sip_message::sniff::{cseq_number, identity, is_response, req_method, resp_status, via_branch};

use super::tally::LateRepair;
use super::taps::Taps;

/// The answers one dialog holds at most: the last ones it sent.
pub(in crate::mux) const ANSWERS_HELD: usize = 8;

/// One final our agent sent, keyed by the server transaction it ends.
struct Answer {
    cseq: u32,
    /// The CSeq method of the request it answers.
    method: String,
    /// The request's top Via branch, echoed on the response.
    branch: String,
    bytes: Vec<u8>,
    /// Where a late re-send of it is reported; `None`: not reported.
    repair: Option<LateRepair>,
}

impl Answer {
    fn answers(&self, cseq: u32, method: &str, branch: &str) -> bool {
        self.cseq == cseq && self.method == method && self.branch == branch
    }
}

/// What a request arriving on a released dialog draws from its held answers.
pub(in crate::mux) enum LateReply {
    /// No held answer is this request's: it is a stray.
    NotHeld,
    /// The ACK of a held INVITE non-2xx final: absorbed.
    Absorbed,
    /// A retransmission of an answered request: the same final again, and
    /// where that re-send is reported.
    Reanswer(Vec<u8>, Option<LateRepair>),
}

/// The answers one released dialog holds.
pub(in crate::mux) struct HeldAnswers(Vec<Answer>);

impl HeldAnswers {
    /// What the request `raw` draws from these answers.
    pub(in crate::mux) fn on_request(&self, raw: &[u8]) -> LateReply {
        let (Some(method), Some(cseq), Some(branch)) =
            (req_method(raw), cseq_number(raw), via_branch(raw))
        else {
            return LateReply::NotHeld;
        };
        if method == "ACK" {
            let acked = self.0.iter().any(|a| a.answers(cseq, "INVITE", &branch));
            return if acked { LateReply::Absorbed } else { LateReply::NotHeld };
        }
        match self.0.iter().find(|a| a.answers(cseq, &method, &branch)) {
            Some(a) => LateReply::Reanswer(a.bytes.clone(), a.repair.clone()),
            None => LateReply::NotHeld,
        }
    }
}

/// An endpoint's answers, per dialog (Call-ID).
#[derive(Default)]
pub(in crate::mux) struct SentAnswers(Mutex<HashMap<String, Vec<Answer>>>);

impl SentAnswers {
    /// Hold `raw` when it is a final this endpoint's agent sends that a
    /// retransmitted request may draw again; it replaces an earlier answer to
    /// the same transaction. `taps` resolve the request's ladder arrow.
    pub(in crate::mux) fn on_outbound(&self, raw: &[u8], taps: Option<&Taps>) {
        // The start line first: a request or a provisional reads no header.
        if !is_response(raw) || !resp_status(raw).is_some_and(|s| s >= 200) {
            return;
        }
        let id = identity(raw);
        let (Some(status), Some(cid), Some(cseq), Some(method)) =
            (id.status, id.call_id, id.cseq, id.cseq_method)
        else {
            return;
        };
        let held = if method == "INVITE" { status >= 300 } else { status >= 200 };
        if !held {
            return;
        }
        let Some(branch) = via_branch(raw) else { return };
        let repair = taps.and_then(|t| t.late_repair(&id));
        let answer =
            Answer { cseq, method: method.to_string(), branch, bytes: raw.to_vec(), repair };
        let mut g = self.0.lock().unwrap();
        let dialog = g.entry(cid.to_string()).or_default();
        dialog.retain(|a| !a.answers(answer.cseq, &answer.method, &answer.branch));
        if dialog.len() == ANSWERS_HELD {
            dialog.remove(0);
        }
        dialog.push(answer);
    }

    /// The answers of dialog `call_id`, removed.
    pub(in crate::mux) fn take(&self, call_id: &str) -> Option<HeldAnswers> {
        self.0.lock().unwrap().remove(call_id).map(HeldAnswers)
    }
}

#[cfg(test)]
#[path = "late_answer_tests.rs"]
mod tests;

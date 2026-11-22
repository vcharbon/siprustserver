//! A load call's departure log: every datagram its UAs put on the wire, in
//! the order it left, the copies its loss model dropped excluded. A lane's
//! datagrams reach the peer in that order, so the log tells whether a request
//! a UA sent reached the peer ahead of the UA's own final (RFC 3261 §12.3,
//! §17.1.2.2) where no recording shows the peer's arrivals.

use std::sync::{Arc, Mutex};

use sip_message::sniff::Identity;
use sip_message::SipResponse;

use super::wire_crossing::{DialogEnd, Ending};

/// The departures of one call's UAs, in the order they left; a clone notes
/// into the same log.
#[derive(Clone, Default)]
pub struct Departures(Arc<Mutex<Vec<Departed>>>);

/// What names one departed datagram: its request's transaction, or the
/// response's request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Departed {
    call_id: String,
    cseq: u32,
    cseq_method: String,
    /// The request method; `None` for a response.
    method: Option<String>,
    /// The response status; `None` for a request.
    status: Option<u16>,
    /// The top-Via branch.
    branch: Option<String>,
}

impl Departures {
    /// Note a datagram of identity `id` leaving for the wire.
    pub fn note(&self, id: &Identity<'_>) {
        let owned = |s: Option<&str>| s.map(str::to_string);
        self.0.lock().unwrap().push(Departed {
            call_id: id.call_id.unwrap_or_default().to_string(),
            cseq: id.cseq.unwrap_or_default(),
            cseq_method: id.cseq_method.unwrap_or_default().to_string(),
            method: owned(id.method),
            status: id.status,
            branch: owned(id.branch),
        });
    }

    /// Whether a copy of the request `answer` answers (its Call-ID, CSeq,
    /// method and top-Via branch) left before the first departed copy of the
    /// UA's own final `end`; `None` when no copy of it left, or `end` is the
    /// peer's own final, which never leaves through the UA.
    pub(super) fn ahead(&self, answer: &SipResponse, end: DialogEnd) -> Option<bool> {
        let (Ending::Final { invite_cseq, status }, false) = (end.ending, end.by_peer) else {
            return None;
        };
        let call_id = answer.call_id().as_str();
        let method = answer.cseq().method().as_str();
        let branch = answer.top_via().branch();
        let log = self.0.lock().unwrap();
        let is_request = |d: &&Departed| {
            d.method.as_deref() == Some(method)
                && d.call_id == call_id
                && d.cseq == answer.cseq().seq()
                && d.branch.as_deref() == branch
        };
        let is_final = |d: &&Departed| {
            d.status == Some(status)
                && d.cseq_method == "INVITE"
                && d.cseq == invite_cseq
                && d.call_id == call_id
        };
        let left = log.iter().position(|d| is_request(&d))?;
        Some(log.iter().position(|d| is_final(&d)).is_none_or(|at| left < at))
    }
}

impl std::fmt::Debug for Departures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Departures({})", self.0.lock().unwrap().len())
    }
}

#[cfg(test)]
mod tests;

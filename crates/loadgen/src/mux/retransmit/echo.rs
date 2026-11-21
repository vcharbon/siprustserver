//! An echo peer ([`NetFaults::echo_rate`](crate::mux::NetFaults)): an endpoint
//! that answers each reaction to its message with that message again. It sends
//! its final to an INVITE again on every ACK of it, and a request again on
//! every final to it. A compliant answerer answers a peer's copies of one
//! message up to the ladder's re-sends at any pace and past them at most one
//! per T1 within 64·T1 (RFC 3261 §13.3.1.4, §17.2.1, §17.2.2; ADR-0063 §1),
//! so the echoes of one message stay under [`ECHO_CAP`]. Reaching it is an
//! overrun: the answerer loops with the peer.

use std::collections::HashMap;
use std::time::Duration;

use sip_message::sniff::Identity;

use super::resender::Sent;
use super::txn_key;
use crate::mux::datagram::key_of;

/// The echoes of one message, at most: above the 2xx ladder's 10 re-sends
/// plus one copy per T1 over 64·T1 at the slowest [`PACES`] (about 64).
pub(super) const ECHO_CAP: u32 = 80;

/// How long an echo peer waits before each echo: at once, or just past T1,
/// where a compliant answerer answers every copy until 64·T1.
pub(crate) const PACES: [Duration; 2] = [Duration::ZERO, Duration::from_millis(600)];

/// An echo peer's messages and the echoes each drew.
pub(super) struct Echo {
    pub(super) pace: Duration,
    /// Our finals to an INVITE, by Call-ID, CSeq and To-tag: what its ACK names.
    finals: HashMap<(u64, u32), Sent>,
    /// Our requests other than ACK, by transaction.
    requests: HashMap<u64, Sent>,
    echoes: HashMap<u64, u32>,
    overrun: bool,
}

/// What one inbound datagram makes the echo peer do.
pub(super) enum Verdict {
    Echo(Sent),
    /// The first echo past [`ECHO_CAP`] of a message: none is sent.
    Overrun,
    None,
}

fn final_key(id: &Identity<'_>) -> Option<(u64, u32)> {
    let (cid, cseq) = id.call_id.zip(id.cseq)?;
    Some((key_of((cid, id.to_tag.unwrap_or_default())), cseq))
}

impl Echo {
    pub(super) fn new(pace: Duration) -> Self {
        Self {
            pace,
            finals: HashMap::new(),
            requests: HashMap::new(),
            echoes: HashMap::new(),
            overrun: false,
        }
    }

    /// Keep `sent`, a datagram of identity `id`, if a reaction to it is echoed.
    pub(super) fn on_outbound(&mut self, id: &Identity<'_>, sent: &Sent) {
        if let Some(status) = id.status {
            if status >= 200 && id.cseq_method == Some("INVITE") {
                if let Some(key) = final_key(id) {
                    self.finals.insert(key, sent.clone());
                }
            }
            return;
        }
        let (Some(method), Some(branch)) = (id.method, id.branch) else { return };
        if method != "ACK" {
            self.requests.insert(txn_key(branch, method), sent.clone());
        }
    }

    /// The echo the inbound datagram of identity `id` draws: an ACK of our
    /// final, or a final to our request.
    pub(super) fn on_inbound(&mut self, id: &Identity<'_>) -> Verdict {
        let (key, sent) = if id.method == Some("ACK") {
            let Some(key) = final_key(id) else { return Verdict::None };
            (key_of(("ack", key)), self.finals.get(&key))
        } else if id.status.is_some_and(|s| s >= 200) {
            let (Some(branch), Some(method)) = (id.branch, id.cseq_method) else {
                return Verdict::None;
            };
            let key = txn_key(branch, method);
            (key, self.requests.get(&key))
        } else {
            return Verdict::None;
        };
        let Some(sent) = sent.cloned() else { return Verdict::None };
        let echoes = self.echoes.entry(key).or_default();
        *echoes += 1;
        if *echoes <= ECHO_CAP {
            Verdict::Echo(sent)
        } else if *echoes == ECHO_CAP + 1 && !self.overrun {
            self.overrun = true;
            Verdict::Overrun
        } else {
            Verdict::None
        }
    }
}

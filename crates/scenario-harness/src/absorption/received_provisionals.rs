//! RFC 3261 §13.2.2.1 for the provisionals this endpoint receives: the client
//! transaction passes every one up (§17.1.1.2), and one byte-identical to a
//! provisional that arrived before it — a node re-sending the provisional its
//! record holds — tells the UAC core nothing new.

use std::collections::HashMap;
use std::sync::Mutex;

use sip_message::SipResponse;

/// A provisional's Call-ID, top-Via branch, CSeq number and status.
type Key = (String, String, u32, u16);

/// Each distinct datagram received under one key, with how many times it
/// arrived.
type Copies = Vec<(Vec<u8>, u32)>;

/// The provisionals received.
#[derive(Default)]
pub(super) struct ReceivedProvisionals {
    seen: Mutex<HashMap<Key, Copies>>,
}

impl ReceivedProvisionals {
    /// Count `r` when it is a provisional other than 100.
    pub(super) fn sight(&self, raw: &[u8], r: &SipResponse) {
        let Some(key) = key(r) else { return };
        let mut seen = self.seen.lock().unwrap();
        let copies = seen.entry(key).or_default();
        match copies.iter_mut().find(|(bytes, _)| bytes.as_slice() == raw) {
            Some((_, n)) => *n += 1,
            None => copies.push((raw.to_vec(), 1)),
        }
    }

    /// Whether `r` is a provisional whose exact bytes arrived before it.
    pub(super) fn repeats(&self, r: &SipResponse) -> bool {
        let Some(key) = key(r) else { return false };
        let seen = self.seen.lock().unwrap();
        seen.get(&key).is_some_and(|copies| {
            copies.iter().any(|(bytes, n)| *n > 1 && bytes.as_slice() == r.image().as_ref())
        })
    }
}

/// A provisional's key; `None` for a final or a 100.
fn key(r: &SipResponse) -> Option<Key> {
    let status = r.status();
    if !(101..200).contains(&status) {
        return None;
    }
    let branch = r.top_via().branch().unwrap_or_default().to_string();
    Some((r.call_id().as_str().to_string(), branch, r.cseq().seq(), status))
}

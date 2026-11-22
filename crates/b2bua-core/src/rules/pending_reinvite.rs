//! A re-INVITE toward the originator that waits in a service's state for her
//! ACK of a 2xx this stack sent her (RFC 3261 §14.1, ADR-0064 §3): what it
//! keeps of the message whose description it offers, and the lines it reads
//! back when it leaves.

use call::PendingResync;
use sip_message::{SipHeader, SipStr};

/// The re-INVITE offering `body`, written by the peer of `leg_id`, with the
/// lines of `headers` that describe it or state its sender's advertisement.
pub(crate) fn park(leg_id: &str, body: &[u8], headers: &[SipHeader]) -> PendingResync {
    let lines = sip_message::generators::body_and_advertisement_lines(headers)
        .into_iter()
        .map(|h| call::SipHeader { name: h.name.to_string(), value: h.value.to_string() })
        .collect();
    PendingResync { leg_id: leg_id.to_string(), body: body.to_vec(), lines }
}

/// The lines `pending` kept, as header lines of the message they came from.
pub(crate) fn lines(pending: &PendingResync) -> Vec<SipHeader> {
    pending
        .lines
        .iter()
        .map(|l| SipHeader { name: SipStr::owned(&l.name), value: SipStr::owned(&l.value) })
        .collect()
}

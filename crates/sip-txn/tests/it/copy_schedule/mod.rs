//! ADR-0063 §1: a transaction answers a peer's copy of its trigger on the RFC
//! schedule. The copies up to the re-sends of the ladder the peer re-sends
//! that trigger on (`Class::resends()`) are answered at any pace, a later one
//! only T1 after the last answered one, and none later than 64·T1 after the
//! trigger's first copy; the copies in between are absorbed. Each test plays a
//! peer that re-sends its trigger far faster than its ladder and counts what
//! the layer puts on the wire for it.

mod client;
mod compliant;
mod server;

use sip_message::SipMessage;

use crate::common::*;

/// The copies of a burst: past every class's re-sends, so a layer that
/// answers each one is told from one that absorbs the rest.
pub const COPIES: usize = 15;

/// Sends `copies` copies of `raw` from the peer, 1 ms apart: a peer
/// re-sending far faster than any ladder, every copy inside T1 of the first.
pub async fn burst(stack: &Stack, raw: &[u8], copies: usize) {
    for _ in 0..copies {
        stack.inject(raw).await;
        elapse_ms(1).await;
    }
}

/// The responses of `status` to a `method` request in a drained batch.
pub fn answers(msgs: &[SipMessage], status: u16, method: &str) -> usize {
    msgs.iter()
        .filter(|m| {
            matches!(m, SipMessage::Response(r)
                if r.status() == status && r.cseq().method() == method)
        })
        .count()
}

/// Sends a copy of `raw` every `every_ms` until `until_ms` have passed,
/// returning for each copy the milliseconds since the first and the
/// responses of `status` to a `method` request it drew.
pub async fn copies_every(
    stack: &Stack,
    raw: &[u8],
    every_ms: u64,
    until_ms: u64,
    status: u16,
    method: &str,
) -> Vec<(u64, usize)> {
    let mut drawn = Vec::new();
    let mut at = 0;
    while at <= until_ms {
        stack.inject(raw).await;
        elapse_ms(every_ms).await;
        drawn.push((at, answers(&stack.drain_peer(), status, method)));
        at += every_ms;
    }
    drawn
}

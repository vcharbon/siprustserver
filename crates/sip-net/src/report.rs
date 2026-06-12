//! Wire-level trace projection — the SIP-specific half of the recording that
//! `layer-harness::scenario` deliberately leaves out (see the note at the top
//! of `layer-harness/src/scenario.rs`: "the SIP-specific projection
//! (`RecordedSipEntry`, the `toSipWire` derivation) lives in `sip-net`, which
//! reads its own typed channel snapshot directly").
//!
//! The recording decorator ([`crate::contracts`]) captures every call as a
//! [`SignalingNetworkEvent`]: a `SendCalled` at the sender and a `RecvItem` at
//! the receiver are the **two halves** of one message crossing the wire. This
//! module pairs them back into a [`RecordedSipEntry`] — one entry per message,
//! carrying the sender/receiver addresses, the raw bytes, and both the sent and
//! (when delivered) received timestamps. A send with no matching receive (lost
//! packet, unbound destination) becomes an entry with `delivered = false`.
//!
//! This is intentionally **byte-level**: it owns no SIP parser. A reporter that
//! wants method/status/Call-ID labels parses `raw` itself with `sip-message`.
//! Keeping the parser out is what lets this projection sit in the network layer.

use std::net::SocketAddr;

use layer_harness::Stamped;

use crate::contracts::SignalingNetworkEvent;

/// One message as it crossed (or tried to cross) the wire, reconstructed from a
/// `SendCalled`/`RecvItem` pair on the recording channel. Port of the source's
/// `NetworkTraceEntry` / report-recorder `RecordedSipEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedSipEntry {
    /// The sender's bound address (the `SendCalled` bind key).
    pub from: SocketAddr,
    /// The destination the sender addressed.
    pub to: SocketAddr,
    /// Exact bytes handed to `send_to` — what the reporter renders as wire text.
    pub raw: Vec<u8>,
    /// When the sender called `send_to` (capture-stamp ms).
    pub sent_ms: u64,
    /// When the receiver observed the datagram, if it was delivered.
    pub received_ms: Option<u64>,
    /// `true` when a matching `RecvItem` was found on the channel.
    pub delivered: bool,
    /// Capture-order tiebreaker from the originating `SendCalled` — the
    /// renderer sorts on `(sent_ms, seq)` exactly like the source's SVG/text
    /// renderers sort on `(timestamp, seq)`.
    pub seq: u64,
}

/// Pair every `SendCalled` on the channel with its delivering `RecvItem` and
/// return one [`RecordedSipEntry`] per send, in capture order.
///
/// A `RecvItem` matches a `SendCalled` when the receiver's bind key is the
/// send's destination, the packet's source is the sender, and the bytes are
/// identical. Each `RecvItem` is consumed by at most one send (so a duplicate
/// retransmission pairs with a distinct delivery), and the earliest matching
/// delivery after the send wins.
///
/// **External peers** (a real SUT outside the recorded network — the kind
/// cluster behind the LB VIP): the recording only sees this process's binds, so
/// a message FROM the cluster has no `SendCalled` half and a message INTO the
/// cluster has no `RecvItem` half. Two adjustments keep the trace truthful:
///   - a `RecvItem` whose `packet.src` is not a recorded bind becomes its OWN
///     entry (`from` = the external sender, `delivered = true`) — without it,
///     everything the cluster sent us (the b-leg INVITE, the 200s) vanished
///     from the trace and every anchor on a received message failed;
///   - a send addressed to a non-recorded destination cannot be judged
///     undelivered (nobody on the recording could have observed the receive),
///     so it is NOT flagged lost. On the fully-recorded fabrics every
///     destination is a recorded bind and the strict semantics are unchanged.
pub fn to_sip_entries(events: &[Stamped<SignalingNetworkEvent>]) -> Vec<RecordedSipEntry> {
    // Every bind the recording observed (BindAcquire plus any event's bind key
    // as a backstop) — the boundary between in-trace and external peers.
    let mut recorded_binds: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();
    for s in events {
        if let Ok(addr) = s.event.bind_key().parse::<SocketAddr>() {
            recorded_binds.insert(addr);
        }
    }

    // Pre-index the deliveries so a send can claim the first unconsumed match.
    let mut recvs: Vec<(usize, SocketAddr, SocketAddr, &[u8], u64, u64)> = Vec::new();
    for s in events {
        if let SignalingNetworkEvent::RecvItem { bind_key, packet } = &s.event {
            if let Ok(receiver) = bind_key.parse::<SocketAddr>() {
                recvs.push((s.seq as usize, receiver, packet.src, &packet.raw, s.at_ms, s.seq));
            }
        }
    }
    let mut consumed = vec![false; recvs.len()];

    let mut out = Vec::new();
    for s in events {
        let SignalingNetworkEvent::SendCalled { bind_key, to, msg } = &s.event else {
            continue;
        };
        let Ok(from) = bind_key.parse::<SocketAddr>() else {
            continue;
        };

        // Earliest unconsumed delivery: receiver == `to`, packet.src == `from`,
        // same bytes, and observed at-or-after this send (seq order).
        let mut matched: Option<usize> = None;
        for (i, (rseq, receiver, src, raw, _, _)) in recvs.iter().enumerate() {
            if consumed[i] {
                continue;
            }
            if *receiver == *to && *src == from && *raw == msg.as_slice() && *rseq >= s.seq as usize
            {
                matched = Some(i);
                break;
            }
        }

        let received_ms = matched.map(|i| {
            consumed[i] = true;
            recvs[i].4
        });

        out.push(RecordedSipEntry {
            from,
            to: *to,
            raw: msg.clone(),
            sent_ms: s.at_ms,
            received_ms,
            // Unmatched + recorded destination ⇒ genuinely lost on the fabric;
            // unmatched + external destination ⇒ left the recording's horizon.
            delivered: received_ms.is_some() || !recorded_binds.contains(to),
            seq: s.seq,
        });
    }

    // Orphan deliveries from EXTERNAL senders: one entry per packet, stamped at
    // its arrival. (An orphan whose src IS a recorded bind — e.g. a fake
    // fabric's pre-ingress synthetic reply — keeps the historic behaviour of
    // not being an entry, so fully-recorded reports are byte-identical.)
    for (i, (_, receiver, src, raw, at_ms, seq)) in recvs.iter().enumerate() {
        if consumed[i] || recorded_binds.contains(src) {
            continue;
        }
        out.push(RecordedSipEntry {
            from: *src,
            to: *receiver,
            raw: raw.to_vec(),
            sent_ms: *at_ms,
            received_ms: Some(*at_ms),
            delivered: true,
            seq: *seq,
        });
    }

    out.sort_by(|a, b| a.sent_ms.cmp(&b.sent_ms).then(a.seq.cmp(&b.seq)));
    out
}

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
//! Arrival vs consumption (newkahneed-036 ask A): a `RecvItem` is the ARRIVAL
//! fact (recorded at delivery into the inbox); a `RecvConsumed` marker is the
//! CONSUMPTION fact (the endpoint's `recv` returned it). An arrival with no
//! matching consumption — or one the inbox/loss-model refused — carries a
//! [`RecvNote`] so renderers can distinguish "the peer sent this and the body
//! ignored it" from "the body expected and matched this".
//!
//! This is intentionally **byte-level**: it owns no SIP parser. A reporter that
//! wants method/status/Call-ID labels parses `raw` itself with `sip-message`.
//! Keeping the parser out is what lets this projection sit in the network layer.

use std::net::SocketAddr;

use layer_harness::Stamped;

use crate::contracts::SignalingNetworkEvent;
use crate::types::RecvDisposition;

/// Why a received message deserves a distinct rendering. `None` on an entry
/// means the normal case: delivered and consumed by the scenario (or a pure
/// send with no receive half).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvNote {
    /// Delivered to the endpoint's inbox but the scenario body never read it.
    Unconsumed,
    /// Arrived but the bounded inbox was full — the app never saw it.
    InboxOverflow,
    /// Arrived after the endpoint closed its inbox.
    InboxClosed,
    /// Discarded by the simulated packet-loss model (loadgen `--drop-rate`).
    LossModel,
    /// Absorbed as a duplicate by the retransmit engine (`--auto-retransmit`).
    AbsorbedRetransmit,
}

impl RecvNote {
    /// Short human tag for renderers (label suffix / badge text).
    pub fn tag(self) -> &'static str {
        match self {
            RecvNote::Unconsumed => "unconsumed",
            RecvNote::InboxOverflow => "inbox overflow",
            RecvNote::InboxClosed => "after close",
            RecvNote::LossModel => "dropped: loss model",
            RecvNote::AbsorbedRetransmit => "absorbed retransmit",
        }
    }
}

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
    /// How the receive half fared beyond plain delivered-and-consumed
    /// (see [`RecvNote`]); `None` for the normal case.
    pub recv_note: Option<RecvNote>,
    /// Capture-order tiebreaker from the originating `SendCalled` — the
    /// renderer sorts on `(sent_ms, seq)` exactly like the source's SVG/text
    /// renderers sort on `(timestamp, seq)`.
    pub seq: u64,
}

struct RecvHalf<'a> {
    seq: u64,
    receiver: SocketAddr,
    bind_key: &'a str,
    src: SocketAddr,
    raw: &'a [u8],
    arrival_ms: u64,
    at_ms: u64,
    disposition: RecvDisposition,
    /// A `RecvConsumed` marker claimed this arrival.
    read: bool,
    /// A `SendCalled` claimed this arrival (pairing state).
    paired: bool,
}

impl RecvHalf<'_> {
    fn note(&self) -> Option<RecvNote> {
        match self.disposition {
            RecvDisposition::Delivered if self.read => None,
            RecvDisposition::Delivered => Some(RecvNote::Unconsumed),
            RecvDisposition::InboxOverflow => Some(RecvNote::InboxOverflow),
            RecvDisposition::InboxClosed => Some(RecvNote::InboxClosed),
            RecvDisposition::LossModel => Some(RecvNote::LossModel),
            RecvDisposition::AbsorbedRetransmit => Some(RecvNote::AbsorbedRetransmit),
        }
    }
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

    // Pre-index the arrivals so a send can claim the first unpaired match.
    let mut recvs: Vec<RecvHalf<'_>> = Vec::new();
    for s in events {
        if let SignalingNetworkEvent::RecvItem { bind_key, packet, disposition } = &s.event {
            if let Ok(receiver) = bind_key.parse::<SocketAddr>() {
                recvs.push(RecvHalf {
                    seq: s.seq,
                    receiver,
                    bind_key,
                    src: packet.src,
                    raw: &packet.raw,
                    arrival_ms: packet.arrival_ms,
                    at_ms: s.at_ms,
                    disposition: *disposition,
                    read: false,
                    paired: false,
                });
            }
        }
    }

    // Claim consumption: each `RecvConsumed` marks the first unread arrival of
    // the SAME packet on the same bind. (The packet clone carries the arrival
    // stamp, so equality is exact; byte-identical retransmits claim in FIFO
    // order, matching the queue's pop order.)
    for s in events {
        let SignalingNetworkEvent::RecvConsumed { bind_key, packet } = &s.event else {
            continue;
        };
        if let Some(r) = recvs.iter_mut().find(|r| {
            !r.read
                && r.bind_key == bind_key
                && r.src == packet.src
                && r.arrival_ms == packet.arrival_ms
                && r.raw == packet.raw.as_slice()
        }) {
            r.read = true;
        }
    }

    let mut out = Vec::new();
    for s in events {
        let SignalingNetworkEvent::SendCalled { bind_key, to, msg } = &s.event else {
            continue;
        };
        let Ok(from) = bind_key.parse::<SocketAddr>() else {
            continue;
        };

        // Earliest unpaired delivery: receiver == `to`, packet.src == `from`,
        // same bytes, and observed at-or-after this send (seq order).
        let matched = recvs.iter_mut().find(|r| {
            !r.paired && r.receiver == *to && r.src == from && r.raw == msg.as_slice() && r.seq >= s.seq
        });

        let (received_ms, recv_note) = match matched {
            Some(r) => {
                r.paired = true;
                (Some(r.at_ms), r.note())
            }
            None => (None, None),
        };

        out.push(RecordedSipEntry {
            from,
            to: *to,
            raw: msg.clone(),
            sent_ms: s.at_ms,
            received_ms,
            // Unmatched + recorded destination ⇒ genuinely lost on the fabric;
            // unmatched + external destination ⇒ left the recording's horizon.
            delivered: received_ms.is_some() || !recorded_binds.contains(to),
            recv_note,
            seq: s.seq,
        });
    }

    // Orphan deliveries from EXTERNAL senders: one entry per packet, stamped at
    // its arrival. (An orphan whose src IS a recorded bind — e.g. a fake
    // fabric's pre-ingress synthetic reply — keeps the historic behaviour of
    // not being an entry, so fully-recorded reports are byte-identical.)
    for r in &recvs {
        if r.paired || recorded_binds.contains(&r.src) {
            continue;
        }
        out.push(RecordedSipEntry {
            from: r.src,
            to: r.receiver,
            raw: r.raw.to_vec(),
            sent_ms: r.at_ms,
            received_ms: Some(r.at_ms),
            delivered: true,
            recv_note: r.note(),
            seq: r.seq,
        });
    }

    out.sort_by(|a, b| a.sent_ms.cmp(&b.sent_ms).then(a.seq.cmp(&b.seq)));
    out
}

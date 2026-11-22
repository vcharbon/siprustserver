//! The typed event union the recording decorator writes to its channel.

use std::net::SocketAddr;

use layer_harness::LaneKey;

use crate::types::{BindSummary, RecvDisposition, SendNote, UdpPacket};
use crate::wire_stamp::WireStamp;

/// One observation on the `SignalingNetwork` typed channel. Every variant
/// carries the lane-identifying `bind_key` so per-peer rules can slice on a
/// single peer.
///
/// `RecvItem` is the ARRIVAL fact, recorded at delivery into the endpoint's
/// inbox (via the [`crate::types::RecvTap`] the decorator installs at bind);
/// `RecvConsumed` is the CONSUMPTION fact, recorded when the endpoint's
/// `recv`/`try_recv` returns the packet. A `RecvItem` with no matching
/// `RecvConsumed` is a message that really crossed the wire but the scenario
/// body never read — still on the ladder, still seen by
/// the RFC audit. On an impl with no tappable inbox the decorator falls back to
/// recording both, adjacently, at recv time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalingNetworkEvent {
    BindAcquire {
        bind_key: LaneKey,
        summary: BindSummary,
    },
    BindRelease {
        bind_key: LaneKey,
    },
    SendCalled {
        bind_key: LaneKey,
        to: SocketAddr,
        msg: Vec<u8>,
    },
    SendResult {
        bind_key: LaneKey,
        outcome: SendOutcome,
    },
    RecvItem {
        bind_key: LaneKey,
        packet: UdpPacket,
        disposition: RecvDisposition,
        wire: WireStamp,
    },
    RecvConsumed {
        bind_key: LaneKey,
        packet: UdpPacket,
    },
    /// An outbound datagram the endpoint's retransmit engine re-emitted below
    /// the recording layer (loadgen `--auto-retransmit`). Projection-only — it
    /// renders on the ladder as a tagged outbound frame but is invisible to the
    /// RFC audit (a re-emit is a byte-identical retransmission the rules dedup).
    ReEmit {
        bind_key: LaneKey,
        to: SocketAddr,
        msg: Vec<u8>,
        kind: crate::types::ReEmitKind,
    },
    /// The `SendCalled` of these bytes, from `bind_key` to `to`, that the
    /// endpoint's loss model then dropped (loadgen `--drop-rate`, the
    /// simulated fabric's [`Fate::Lose`](crate::Fate::Lose)): it never
    /// reached its taker. Rules never judge it; the projection renders its
    /// send lost and the cross pass reads it as loss evidence.
    SendLost {
        bind_key: LaneKey,
        to: SocketAddr,
        msg: Vec<u8>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Ok,
    Err,
}

impl SignalingNetworkEvent {
    /// The event of `msg`, which the endpoint of `bind_key` handled below the
    /// recording as `note` says: a `ReEmit`, or the `SendLost` of a send its
    /// loss model dropped.
    pub(crate) fn sent_below(
        bind_key: LaneKey,
        to: SocketAddr,
        msg: Vec<u8>,
        note: SendNote,
    ) -> Self {
        match note {
            SendNote::ReEmit(kind) => SignalingNetworkEvent::ReEmit { bind_key, to, msg, kind },
            SendNote::LossModel => SignalingNetworkEvent::SendLost { bind_key, to, msg },
        }
    }

    pub fn bind_key(&self) -> &LaneKey {
        match self {
            SignalingNetworkEvent::BindAcquire { bind_key, .. }
            | SignalingNetworkEvent::BindRelease { bind_key }
            | SignalingNetworkEvent::SendCalled { bind_key, .. }
            | SignalingNetworkEvent::SendResult { bind_key, .. }
            | SignalingNetworkEvent::RecvItem { bind_key, .. }
            | SignalingNetworkEvent::RecvConsumed { bind_key, .. }
            | SignalingNetworkEvent::ReEmit { bind_key, .. }
            | SignalingNetworkEvent::SendLost { bind_key, .. } => bind_key,
        }
    }
}

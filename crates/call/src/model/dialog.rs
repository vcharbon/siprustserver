//! RFC 3261 §12 dialog state plus the B2BUA-only dialog extensions
//! ([`B2buaDialogExt`]) — pending transparent relays, cached SDP, the retained
//! emissions the dialog's retransmission obligations repeat. The enclosing
//! per-leg record is [`Leg`](crate::model::Leg).

use serde::{Deserialize, Serialize};

use super::emission::RetainedEmission;
use super::invite_txn::InviteTxnHandle;

/// Direction of an original relayed request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    FromA,
    FromB,
}

/// Snapshot of an inbound request the B2BUA relays transparently, stored on the
/// target-leg dialog so its response can be rebuilt with the right
/// Via/From/To/Call-ID/CSeq (RFC 3261 §8.1.3.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequest {
    pub method: String,
    pub outbound_cseq: i64,
    pub inbound_cseq: i64,
    pub source_vias: Vec<String>,
    pub source_call_id: String,
    pub source_from: String,
    pub source_to: String,
    /// The requester's `Timestamp`, held so the response echoes the value the
    /// request carried (RFC 3261 §8.2.6.1); absent when the request carried none.
    #[serde(default)]
    pub source_timestamp: Option<String>,
    pub direction: Direction,
    /// The originator CANCELled this relayed (re-)INVITE (RFC 3261 §9): the
    /// B2BUA CANCELled the outbound client transaction and the txn layer
    /// already answered the originator (200 + 487), so the target's eventual
    /// final response is resolved locally — never relayed. A crossing 2xx
    /// (target answered before the CANCEL landed) is ACKed and absorbed.
    #[serde(default)]
    pub cancelled: bool,
    /// The originator's request offered reliable provisionals — `100rel` in its
    /// own `Require` or `Supported` (RFC 3262 §3). The request this stack built
    /// toward the target may offer it where the originator did not (a declared
    /// advertisement), so the originator's licence is recorded here: a
    /// reliable provisional relays reliably only under it. `false` when
    /// hydrated from a peer that recorded none: an unwitnessed offer is no
    /// offer, so the provisional relays unreliably and this stack acknowledges
    /// the responder itself. Trailing under the positional codec.
    #[serde(default)]
    pub offered_100rel: bool,
}

/// RFC 3261 §12 dialog state, stack-owned. `localTag` is the B2BUA's tag on this
/// leg; `remoteTag` is the peer's. `callId`/`localUri`/`remoteUri` are
/// denormalised from the enclosing leg so generators need no leg context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackDialog {
    pub call_id: String,
    pub local_tag: String,
    pub remote_tag: String,
    pub local_uri: String,
    pub remote_uri: String,
    /// Peer Contact URI — Request-URI for in-dialog requests (§12.2.1.1).
    pub remote_target: String,
    /// Last-sent CSeq on this dialog (§8.1.1.5).
    pub local_cseq: i64,
    /// Outbound route set from the dialog-creating response (§12.1.2).
    pub route_set: Vec<String>,
}

/// A 2xx to an INVITE this side sent and still awaits the ACK for (RFC 3261
/// §13.3.1.4), held on the dialog it was sent on: the retained emission its
/// ladder repeats, with the two wire facts the discharging ACK carries — so
/// the ACK is matched on what the 2xx said, not on what the dialog records.
/// While set, the dialog's INVITE server transaction is in the RFC 6026
/// *Accepted* state, so a new INVITE on either face is glare (§14.1) and gets
/// 491. The ladder is always armed (ADR-0029 X5), so the marker lives exactly
/// as long as its give-up: the discharging ACK, the give-up, or the call's end
/// clears it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unacked2xx {
    /// The To-tag the 2xx carried — the discharging ACK's To-tag.
    pub dialog_tag: String,
    /// The CSeq of the INVITE the 2xx answers — the discharging ACK's CSeq.
    /// Only an ACK echoing this number discharges the obligation, so a
    /// retransmitted *initial* ACK cannot discharge a re-INVITE's.
    pub cseq: i64,
    /// The 2xx as it left, on the §13.3.1.4 ladder ([`super::Repeat::Paced`]).
    pub emission: RetainedEmission,
}

/// B2BUA-only dialog extensions that never surface to the SIP stack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct B2buaDialogExt {
    /// Remote party's highest CSeq. `None` until first message from remote.
    pub remote_cseq: Option<i64>,
    /// Pending transparently-relayed inbound requests awaiting response here.
    pub inbound_pending_requests: Vec<PendingRequest>,
    /// Via branch of the first ACK for this dialog's 2xx (§13.2.2.4 re-ACK).
    pub ack_branch: Option<String>,
    /// In-flight re-INVITE client transaction handle on this dialog.
    pub pending_invite_txn: Option<InviteTxnHandle>,
    /// SDP cached from a reliable 18x / UPDATE under the `fake-prack` strategy.
    #[serde(with = "serde_bytes")]
    pub cached_sdp: Option<Vec<u8>>,
    /// RFC 3261 §13.3.1.4 (in-dialog) — a re-INVITE 2xx this side sent that
    /// still awaits the originator's ACK, on the originator's own dialog.
    /// `None` normally; see [`Unacked2xx`].
    #[serde(default)]
    pub pending_reinvite_2xx: Option<Unacked2xx>,
    /// CSeq (in the acknowledging peer's own sequence space) of the ACK this
    /// dialog's 2xx still awaits from that peer (RFC 3261 §13.2.2.4): armed when
    /// the 2xx is taken, discharged by the ACK actually leaving — whether this
    /// stack composed it on receipt or relayed the peer's. `None` = nothing owed,
    /// so a further peer ACK is absorbed rather than put on a quiesced
    /// transaction; a retransmitted 2xx re-ACKs via `ack_branch` without
    /// consulting this.
    #[serde(default)]
    pub awaited_ack_cseq: Option<i64>,
    /// RFC 3261 §13.3.1.4 — the initial-INVITE 2xx this call answered the
    /// caller with, as the exact datagram the retransmit ladder repeats
    /// ([`Unacked2xx`]; a-leg dialog only). Captured at the one seam every
    /// a-facing answer is emitted through. `None` until answered, and again
    /// once the caller's ACK discharges it — retained bytes are never sent
    /// after that, and a later re-answer through the seam retains its own
    /// datagram afresh.
    #[serde(default)]
    pub answered_2xx: Option<Unacked2xx>,
    /// RFC 3261 §13.2.2.4 — the ACK emitted on this dialog's current INVITE
    /// transaction, as the exact datagram a retransmitted 2xx re-sends
    /// (`re-ack-retransmitted-2xx`; [`RetainedEmission`] repeated on trigger).
    /// §13.2.2.4 hands *the* ACK back to the transport for every copy of the
    /// 2xx: on a delayed offer it carries the answer only that ACK supplied
    /// (RFC 3264 §4). `None` until that ACK leaves, and again once a new INVITE
    /// transaction resets it alongside `ack_branch` — the bytes belong to
    /// exactly one INVITE's ACK.
    #[serde(default)]
    pub emitted_ack: Option<RetainedEmission>,
}

/// Composite Dialog = stack §12 state + B2BUA-only extensions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dialog {
    pub sip: StackDialog,
    pub ext: B2buaDialogExt,
}

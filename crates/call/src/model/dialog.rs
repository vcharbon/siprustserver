//! RFC 3261 §12 dialog state plus the B2BUA-only dialog extensions
//! ([`B2buaDialogExt`]) — pending transparent relays, cached SDP, the re-INVITE
//! 2xx retransmit obligation. The enclosing per-leg record is
//! [`Leg`](crate::model::Leg).

use serde::{Deserialize, Serialize};

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

/// The a-leg re-INVITE **2xx** the B2BUA relayed to the originator and is still
/// awaiting the originator's ACK for (RFC 3261 §13.3.1.4, in-dialog). Cached at
/// relay time on the a-leg dialog so the re-INVITE un-ACKed-2xx watchdog can
/// re-send a **byte-faithful** copy raw until the ACK arrives — the re-INVITE
/// analogue of the initial-INVITE retransmit, which rebuilds from the never-
/// mutated `a_leg_invite` + `cached_sdp`. A re-INVITE 2xx *cannot* be rebuilt
/// from the initial snapshot (wrong CSeq, different answer SDP), so the exact
/// serialized outbound bytes are stored instead. Cleared when the a-leg ACK
/// (matching [`cseq`](Self::cseq)) arrives; `None` when no a-leg re-INVITE
/// awaits its ACK. Replicated like `cached_sdp`, so a takeover node keeps the
/// retransmit obligation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReinvite2xx {
    /// The exact serialized 2xx response emitted to the a-leg (byte-faithful
    /// retransmit — same To-tag, SDP, Contact, Allow/Supported, CSeq).
    #[serde(with = "serde_bytes")]
    pub response: Vec<u8>,
    /// Wire destination of the retransmit (the a-leg's address — the 2xx's
    /// top-Via sent-by, computed once at relay time).
    pub dest_host: String,
    pub dest_port: u16,
    /// CSeq the originator's ACK will carry (the a-leg re-INVITE's CSeq). The
    /// watchdog is quiesced only by an ACK echoing this number, so a
    /// retransmitted *initial* ACK cannot prematurely cancel it.
    pub cseq: i64,
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
    /// RFC 3261 §13.3.1.4 (in-dialog) — the a-leg re-INVITE 2xx awaiting the
    /// originator's ACK, on the a-leg dialog only. `None` normally; see
    /// [`PendingReinvite2xx`]. Trailing field with `#[serde(default)]` so it is
    /// decode-tolerant of a body encoded before it existed.
    #[serde(default)]
    pub pending_reinvite_2xx: Option<PendingReinvite2xx>,
    /// The `(name, value)` advertisement lines the initial-INVITE 2xx carried to
    /// the originator, cached beside `cached_sdp` at answer time. RFC 3261
    /// §13.3.1.4 makes a retransmit a copy of the response it retransmits, and
    /// the set resolved then — the callee's own relayed one, or a firing rule's
    /// — is not derivable from the a-leg snapshot. Empty until answered.
    #[serde(default)]
    pub answered_advert: Vec<(String, String)>,
}

/// Composite Dialog = stack §12 state + B2BUA-only extensions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dialog {
    pub sip: StackDialog,
    pub ext: B2buaDialogExt,
}

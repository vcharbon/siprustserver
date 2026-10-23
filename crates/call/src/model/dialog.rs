//! One RFC 3261 §12 dialog of a leg ([`Dialog`]): its identity (the two tags;
//! the Call-ID is the leg's), its state ([`DialogState`]), the two CSeq
//! sequences, the remote target and route set, and what the B2BUA keeps on it —
//! pending transparent relays, cached SDP, the retained emissions its
//! retransmission obligations repeat. The enclosing record is
//! [`Leg`](crate::model::Leg); a leg's dialogs are reached through
//! [`Dialogs`](crate::model::Dialogs).

use serde::{Deserialize, Serialize};

use super::emission::RetainedEmission;
use super::invite_txn::InviteTxnHandle;
use super::owed_answer::OwedAnswer;
use super::own_notify::OwnNotify;
use super::relay_owner::RelayOwner;
use super::sdp_session::LegSdpSession;

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
    pub direction: Direction,
    /// The originator CANCELled this relayed (re-)INVITE (RFC 3261 §9): the
    /// B2BUA CANCELled the outbound client transaction and the txn layer
    /// already answered the originator (200 + 487), so the target's eventual
    /// final response is resolved locally — never relayed. A crossing 2xx
    /// (target answered before the CANCEL landed) is ACKed and absorbed.
    pub cancelled: bool,
    /// The originator's request offered reliable provisionals — `100rel` in its
    /// own `Require` or `Supported` (RFC 3262 §3). The request this stack built
    /// toward the target may offer it where the originator did not (a declared
    /// advertisement), so the originator's licence is recorded here: a
    /// reliable provisional relays reliably only under it; without it the
    /// provisional relays unreliably and this stack acknowledges the responder
    /// itself.
    pub offered_100rel: bool,
    /// The originator's request carried a description (RFC 3264): an offer,
    /// on an INVITE or an UPDATE. An offerless INVITE has its offer made by
    /// the answerer.
    #[serde(default)]
    pub offered: bool,
    /// The originator's request's `Timestamp` value, which the response relayed
    /// back to it echoes (RFC 3261 §8.2.6.1); `None` where it stated none.
    #[serde(default)]
    pub source_timestamp: Option<String>,
    /// The worker that relayed the request, by ordinal and worker identity:
    /// its client transaction carries the relay, and the final returns there
    /// (RFC 3261 §18.2.2). The default names no worker.
    #[serde(default)]
    pub relayed_by: RelayOwner,
}

impl PendingRequest {
    /// A relay of `method` sent at `outbound_cseq` for the originator's
    /// request at `inbound_cseq`, coming `direction`; nothing else is known of
    /// it yet.
    pub fn new(method: &str, outbound_cseq: i64, inbound_cseq: i64, direction: Direction) -> Self {
        PendingRequest {
            method: method.into(),
            outbound_cseq,
            inbound_cseq,
            source_vias: Vec::new(),
            source_call_id: String::new(),
            source_from: String::new(),
            source_to: String::new(),
            direction,
            cancelled: false,
            offered_100rel: false,
            offered: false,
            source_timestamp: None,
            relayed_by: RelayOwner::default(),
        }
    }
}

/// A 2xx to an INVITE this side sent and still awaits the ACK for (RFC 3261
/// §13.3.1.4), held on the dialog it was sent on: the retained emission its
/// ladder repeats, with the two wire facts the discharging ACK carries — so
/// the ACK is matched on what the 2xx said, not on what the dialog records.
/// While set, the dialog's INVITE server transaction is in the RFC 6026
/// *Accepted* state, so a new INVITE on either face is glare (§14.1) and gets
/// 491. The ladder is always armed (ADR-0032 X5), so the marker lives exactly
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

/// Where a dialog stands (RFC 3261 §12, §15). A dialog record is kept in every
/// state until its call record is released, so a late message naming it is
/// answered by what it was, not by its absence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DialogState {
    /// Created by a tagged provisional to the dialog-creating INVITE (§12.1).
    Early,
    /// A 2xx to the dialog-creating INVITE established it (§12.1).
    Confirmed,
    /// This side sent the request that ends the dialog, and it has no final
    /// yet: the dialog still exists (§15.1.2).
    Ending(EndingBy),
    /// The dialog is over; how it ended.
    Ended(DialogEnd),
    /// An early dialog the call let go of without a BYE: a losing callee fork
    /// or a caller-facing dialog the answer did not take (§12.1.2).
    Retired(RetiredDialog),
}

/// The request this side sent to end a dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndingBy {
    /// A BYE on the confirmed dialog (§15.1.1).
    Bye,
    /// A CANCEL of the dialog-creating INVITE (§9.1): the early dialog ends on
    /// that INVITE's final.
    Cancel,
}

/// How a dialog ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DialogEnd {
    /// The dialog-creating INVITE drew this non-2xx final (§12.1, §13.2.2.3).
    Rejected(u16),
    /// The peer sent a BYE on the dialog a 2xx established (§15.1.2).
    PeerBye,
    /// The peer sent a BYE on the dialog while it was early: no 2xx ever
    /// established it (§15; a callee's is a deviation).
    EarlyPeerBye,
    /// This side's BYE completed: `final_status` is its final, `None` when
    /// its transaction timed out (§15.1.1).
    OurBye { final_status: Option<u16> },
}

/// What a retired dialog still carries: the relays it holds open until their
/// finals reach their originators (§8.1.3.3), and the release this stack sent
/// when the dialog, as a losing fork, answered with a 2xx anyway (§13.2.2.4).
/// The dialog's own `our_cseq` stays the sequence that release continues.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RetiredDialog {
    /// Requests relayed into the dialog before it retired, awaiting finals.
    pub open_relays: Vec<PendingRequest>,
    /// The ACK and BYE this stack answered the dialog's late 2xx with.
    pub release: Option<StragglerRelease>,
}

/// The release of a retired fork's 2xx (RFC 3261 §13.2.2.4): every repeat of
/// that 2xx re-sends the ACK on `ack_branch`; the BYE on `bye_branch` ends the
/// dialog, and its final belongs to the release alone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StragglerRelease {
    pub ack_branch: String,
    pub bye_branch: String,
    /// The BYE's transaction ended: a final, or its timeout (§15.1.1).
    pub settled: bool,
}

impl RetiredDialog {
    /// Whether this stack's release BYE of the dialog awaits its final: the
    /// dialog then reads as ending (§15.1.2).
    pub fn release_in_flight(&self) -> bool {
        self.release.as_ref().is_some_and(|r| !r.settled)
    }
}

/// A callee dialog a caller-facing dialog stands for: `leg_id`'s dialog
/// whose peer tag is `peer_tag`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDialog {
    pub leg_id: String,
    pub peer_tag: String,
}

/// The two tags that name a dialog within its leg (RFC 3261 §12): with the
/// leg's Call-ID, the dialog ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DialogId {
    pub our_tag: String,
    pub peer_tag: String,
}

/// One dialog of a leg (RFC 3261 §12). Its identity is the leg's Call-ID plus
/// `our_tag` and `peer_tag`: a request received on it carries `our_tag` as its
/// To-tag and `peer_tag` as its From-tag; a response, the reverse.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dialog {
    /// This stack's tag in the dialog.
    pub our_tag: String,
    /// The peer's tag in the dialog.
    pub peer_tag: String,
    pub state: DialogState,
    /// The last CSeq this side sent in the dialog; 0 before its first. Every
    /// request this side sends in it, relayed or its own, carries the next
    /// number (§12.2.1.1): the number a relayed request arrived with plays
    /// no part.
    pub our_cseq: i64,
    /// The last CSeq the peer sent in the dialog; `None` until it sent one
    /// (§12.2.2). It serves the out-of-order check alone.
    pub peer_cseq: Option<i64>,
    /// Peer Contact URI — Request-URI for in-dialog requests (§12.2.1.1).
    pub remote_target: String,
    /// Route set from the dialog-creating message (§12.1.1, §12.1.2).
    pub route_set: Vec<String>,
    /// On a caller-facing dialog, the callee dialogs it relays, the one a
    /// provisional last showed under it last: one, or several a masking
    /// policy shows the caller as one dialog. Empty for a dialog this stack
    /// answers itself.
    pub fronts: Vec<PeerDialog>,
    /// A provisional this stack sent the caller carried this dialog's
    /// `our_tag`, so she holds the early dialog it names (§12.1.1): a
    /// caller-facing dialog only, set once and kept.
    pub shown: bool,
    /// On a caller-facing dialog, the media leg whose early media it shows:
    /// that dialog is the media leg's session alone, so no callee is shown
    /// under it (RFC 3264 §8). A fact about what the caller sees, not a relay
    /// route: requests on the dialog go where [`Self::fronts`] sends them.
    #[serde(default)]
    pub shows_media: Option<String>,
    /// Pending transparently-relayed inbound requests awaiting response here.
    pub inbound_pending_requests: Vec<PendingRequest>,
    /// Via branch of the first ACK for this dialog's 2xx (§13.2.2.4 re-ACK).
    pub ack_branch: Option<String>,
    /// In-flight INVITE client transaction handle on this dialog.
    pub pending_invite_txn: Option<InviteTxnHandle>,
    /// Under the `fake-prack` strategy, the description a 2xx without one gives
    /// the caller: the reliable 18x's answer, or her offer answered from the
    /// latest early UPDATE offer.
    #[serde(with = "serde_bytes")]
    pub cached_sdp: Option<Vec<u8>>,
    /// RFC 3261 §13.3.1.4 (in-dialog) — a re-INVITE 2xx this side sent that
    /// still awaits the originator's ACK, on the originator's own dialog.
    /// `None` normally; see [`Unacked2xx`].
    pub pending_reinvite_2xx: Option<Unacked2xx>,
    /// CSeq (in the acknowledging peer's own sequence space) of the ACK this
    /// dialog's 2xx still awaits from that peer (RFC 3261 §13.2.2.4): armed when
    /// the 2xx is taken, discharged by the ACK actually leaving — the peer's
    /// relayed, or one this stack composed on its own account. `None` = nothing
    /// owed, so a further peer ACK is absorbed rather than put on a quiesced
    /// transaction; a retransmitted 2xx re-ACKs via `ack_branch` without
    /// consulting this. While it is armed the dialog is in RFC 6026 *Accepted*,
    /// so a newcomer INVITE there is glare (`invite_transaction_open`).
    pub awaited_ack_cseq: Option<i64>,
    /// RFC 3261 §13.3.1.4 — the initial-INVITE 2xx this call answered the
    /// caller with, as the exact datagram the retransmit ladder repeats
    /// ([`Unacked2xx`]; caller-facing dialog only). Captured at the one seam
    /// every a-facing answer is emitted through. `None` until answered, and
    /// again once the caller's ACK discharges it — retained bytes are never
    /// sent after that, and a later re-answer through the seam retains its own
    /// datagram afresh.
    pub answered_2xx: Option<Unacked2xx>,
    /// RFC 3261 §13.2.2.4 — the ACK emitted on this dialog's current INVITE
    /// transaction, as the exact datagram a retransmitted 2xx re-sends
    /// (`re-ack-retransmitted-2xx`; [`RetainedEmission`] repeated on trigger).
    /// §13.2.2.4 hands *the* ACK back to the transport for every copy of the
    /// 2xx: on a delayed offer it carries the answer only that ACK supplied
    /// (RFC 3264 §4). `None` until that ACK leaves, and again once a new INVITE
    /// transaction resets it alongside `ack_branch` — the bytes belong to
    /// exactly one INVITE's ACK. For a round the record never held, the ACK of
    /// its non-2xx too, which no transaction here re-sends (ADR-0047).
    pub emitted_ack: Option<RetainedEmission>,
    /// The session this early dialog carries where the stack has described
    /// something inside it before its leg is confirmed: each early dialog of a
    /// fork is its own offer/answer exchange (RFC 3261 §12.1.2, RFC 3264 §4),
    /// and the one that confirms hands this state to its leg. `None` while the
    /// dialog carries what the leg's opening INVITE stated.
    pub sdp_session: Option<LegSdpSession>,
    /// The offer the dialog's responder made that this side still owes an
    /// answer to, and the message that carries it (RFC 3264 §4); `None` when
    /// nothing is owed.
    #[serde(default)]
    pub owed_answer: Option<OwedAnswer>,
    /// This stack's NOTIFY on the dialog that awaits its final, and its
    /// requests held behind it; `None` when no NOTIFY of this stack is
    /// unanswered.
    #[serde(default)]
    pub own_notify: Option<OwnNotify>,
}

impl Dialog {
    /// A dialog named by `our_tag` and `peer_tag`, in `state`, whose own
    /// sequence stands at `our_cseq`; nothing else is known of it yet.
    pub fn new(
        our_tag: impl Into<String>,
        peer_tag: impl Into<String>,
        state: DialogState,
        our_cseq: i64,
    ) -> Dialog {
        Dialog {
            our_tag: our_tag.into(),
            peer_tag: peer_tag.into(),
            state,
            our_cseq,
            peer_cseq: None,
            remote_target: String::new(),
            route_set: Vec::new(),
            fronts: Vec::new(),
            shown: false,
            shows_media: None,
            inbound_pending_requests: Vec::new(),
            ack_branch: None,
            pending_invite_txn: None,
            cached_sdp: None,
            pending_reinvite_2xx: None,
            awaited_ack_cseq: None,
            answered_2xx: None,
            emitted_ack: None,
            sdp_session: None,
            owed_answer: None,
            own_notify: None,
        }
    }

    /// The tags that name the dialog.
    pub fn id(&self) -> DialogId {
        DialogId { our_tag: self.our_tag.clone(), peer_tag: self.peer_tag.clone() }
    }

    /// Whether the dialog is early or confirmed, or this side's BYE or CANCEL
    /// ending it is in flight: the states in which it still exists.
    pub fn is_live(&self) -> bool {
        matches!(self.state, DialogState::Early | DialogState::Confirmed | DialogState::Ending(_))
    }

    /// Whether a new request may ride the dialog: a live one, but not an
    /// early dialog whose INVITE this side is cancelling (§9.1).
    pub fn carries_requests(&self) -> bool {
        self.is_live() && self.state != DialogState::Ending(EndingBy::Cancel)
    }

    /// Whether the dialog awaits the final of the INVITE that created it:
    /// early, or this side's CANCEL of that INVITE is in flight (§9.1).
    pub fn awaits_invite_final(&self) -> bool {
        matches!(self.state, DialogState::Early | DialogState::Ending(EndingBy::Cancel))
    }

    /// Whether a 2xx established the dialog and no BYE has completed on it:
    /// confirmed, or this side's BYE is in flight.
    pub fn is_confirmed(&self) -> bool {
        matches!(self.state, DialogState::Confirmed | DialogState::Ending(EndingBy::Bye))
    }

    /// Whether a 2xx established the dialog: confirmed, ending, or ended by a
    /// BYE since.
    pub fn was_established(&self) -> bool {
        self.is_confirmed()
            || matches!(
                self.state,
                DialogState::Ended(DialogEnd::PeerBye | DialogEnd::OurBye { .. })
            )
    }

    /// The retired dialog's record; `None` in any other state.
    pub fn retired(&self) -> Option<&RetiredDialog> {
        match &self.state {
            DialogState::Retired(r) => Some(r),
            _ => None,
        }
    }

    /// The retired dialog's record, to update; `None` in any other state.
    pub fn retired_mut(&mut self) -> Option<&mut RetiredDialog> {
        match &mut self.state {
            DialogState::Retired(r) => Some(r),
            _ => None,
        }
    }
}

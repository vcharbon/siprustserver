//! One RFC 3261 §12 dialog of a leg ([`Dialog`]): its identity (the two tags;
//! the Call-ID is the leg's), its state ([`DialogState`]), the two CSeq
//! sequences, the remote target and route set, and what the B2BUA keeps on it —
//! the request records open on it, cached SDP, the retained emissions its
//! retransmission obligations repeat. The enclosing record is
//! [`Leg`](crate::model::Leg); a leg's dialogs are reached through
//! [`Dialogs`](crate::model::Dialogs).

use serde::{Deserialize, Serialize};

use super::owed_answer::OwedAnswer;
use super::queued_request::{Behind, Queued, QueuedRequest};
use super::request_record::RequestRecord;
use super::sdp_session::LegSdpSession;
use super::subscription::Subscription;

/// Direction of an original relayed request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    FromA,
    FromB,
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
    /// or a caller-facing dialog the answer did not take (§12.1.2). Its
    /// request records stay in [`Dialog::requests`] until they settle
    /// (§8.1.3.3), the BYE releasing a late 2xx of it among them (§13.2.2.4);
    /// its `our_cseq` stays the sequence such a release continues.
    Retired,
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
    /// The requests open on the dialog other than its INVITE exchanges
    /// (ADR-0055 §2): relayed into it, sent by this stack, or received and
    /// held for a later answer; kept through its retirement until they settle.
    pub requests: Vec<RequestRecord>,
    /// The INVITE this side last sent on the dialog (RFC 3261 §14): a
    /// re-INVITE, relayed or its own, or on a dialog the leg's dialling INVITE
    /// created, that INVITE; with the 2xx it took and the ACK that 2xx owes
    /// (§13.2.2.4). The next INVITE this side sends on the dialog replaces it,
    /// and a non-2xx ending a re-INVITE closes it.
    pub sent_invite: Option<RequestRecord>,
    /// The INVITE this side received on the dialog and answered 2xx, while
    /// that 2xx awaits its ACK (§13.3.1.4): the call's answer on a
    /// caller-facing dialog, or a relayed re-INVITE's. The discharging ACK,
    /// the give-up or the call's end closes it.
    pub answered_invite: Option<RequestRecord>,
    /// Under the `fake-prack` strategy, the description a 2xx without one gives
    /// the caller: the reliable 18x's answer, or her offer answered from the
    /// latest early UPDATE offer.
    #[serde(with = "serde_bytes")]
    pub cached_sdp: Option<Vec<u8>>,
    /// The session this early dialog carries where the stack has described
    /// something inside it before its leg is confirmed: each early dialog of a
    /// fork is its own offer/answer exchange (RFC 3261 §12.1.2, RFC 3264 §4),
    /// and the one that confirms hands this state to its leg. `None` while the
    /// dialog carries what the leg's opening INVITE stated.
    pub sdp_session: Option<LegSdpSession>,
    /// The offer the dialog's responder made that this side still owes an
    /// answer to, and the message that carries it (RFC 3264 §4); `None` when
    /// nothing is owed. A fact of the session: the offer opens in one
    /// transaction and its answer may ride another (ADR-0055 §2).
    #[serde(default)]
    pub owed_answer: Option<OwedAnswer>,
    /// The subscriptions this stack notifies on the dialog (RFC 6665, RFC
    /// 5057 §3), each opened by its first NOTIFY and kept once terminated.
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
    /// This stack's requests on the dialog not yet sent, oldest first, each
    /// with what it waits behind: the NOTIFYs and BYE asked for while one of
    /// its NOTIFYs there awaits its final leave in turn once nothing ahead of
    /// them is in flight (RFC 3261 §12.2.2); a BYE behind the ACK of this
    /// stack's 2xx leaves on that ACK or the 2xx's give-up (§15).
    #[serde(default)]
    pub queued: Vec<Queued>,
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
            requests: Vec::new(),
            sent_invite: None,
            answered_invite: None,
            cached_sdp: None,
            sdp_session: None,
            owed_answer: None,
            subscriptions: Vec::new(),
            queued: Vec::new(),
        }
    }

    /// The subscription of the dialog `event` and `id` name.
    pub fn subscription(&self, event: &str, id: Option<&str>) -> Option<&Subscription> {
        self.subscriptions.iter().find(|s| s.is_named(event, id))
    }

    /// The requests queued on the dialog behind `behind`, oldest first.
    pub fn queued_behind(&self, behind: Behind) -> impl Iterator<Item = &QueuedRequest> {
        self.queued.iter().filter(move |q| q.behind == behind).map(|q| &q.request)
    }

    /// Drop the INVITE the dialog answered: the BYE held behind the ACK of
    /// its 2xx goes with it.
    pub fn drop_answered_invite(&mut self) {
        self.answered_invite = None;
        self.queued.retain(|q| q.behind != Behind::AnswerAck);
    }

    /// When this stack's next request on the dialog may leave though its
    /// NOTIFYs there are unanswered: the latest end of their waits; `None`
    /// while none awaits a final.
    pub fn notify_wait_ends_at_ms(&self) -> Option<i64> {
        self.subscriptions.iter().filter_map(|s| s.notify.map(|n| n.cap_at_ms())).max()
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

    /// Whether the call let the early dialog go ([`DialogState::Retired`]).
    pub fn is_retired(&self) -> bool {
        self.state == DialogState::Retired
    }

    /// Whether this stack's BYE releasing the retired dialog awaits its
    /// final: the dialog then reads as ending (§15.1.2).
    pub fn release_in_flight(&self) -> bool {
        self.is_retired() && self.requests.iter().any(|r| r.is_own() && r.method == "BYE")
    }
}

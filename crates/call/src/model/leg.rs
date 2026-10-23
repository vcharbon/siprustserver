//! Per-leg record ([`Leg`]) and its role enum; its state is derived in
//! [`crate::model::leg_state`]. Dialog
//! internals live in [`crate::model::dialog`]; the enclosing call record in
//! [`crate::model::record`].

use serde::{Deserialize, Serialize};

use super::dialogs::Dialogs;
use super::message_ring::MessageRing;
use super::outbound_invite::OutboundInvite;
use super::sdp_session::LegSdpSession;
use super::services::ExtMap;

/// Remote peer endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub address: String,
    pub port: u16,
}

/// Explicit per-leg role (ADR-0014). Read via [`crate::helpers::leg_kind`],
/// which defaults from `legId` when absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LegKind {
    A,
    Destination,
    Media,
    TransferTarget,
}

/// Whether a dialled leg's callee rang.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ring {
    /// No 180–189 to the leg's INVITE is on record.
    #[default]
    Silent,
    /// The leg's INVITE received a 180–189 provisional.
    Rang,
    /// A restore found none on record for the leg's unanswered INVITE, though
    /// the turn that took one may not have replicated: its ring deadline runs
    /// from the INVITE until an 18x arrives.
    Presumed,
}

impl Ring {
    /// Whether a 180–189 to the leg's INVITE is on record.
    pub fn rang(self) -> bool {
        self == Ring::Rang
    }
}

/// Per-leg state. `legId` is `"a"`, `"b-1"`, `"b-2"`, …
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    pub leg_id: String,
    pub call_id: String,
    pub source: RemoteInfo,
    /// The leg's dialogs in every state: several while early (forking), one
    /// confirmed at most.
    pub dialogs: Dialogs,
    /// B2BUA's local address on this leg (§12.1.1's local URI) as the
    /// dialog-creating INVITE stated it, display name included, tag excluded:
    /// the From of every request this side sends in the leg's dialogs.
    pub local_uri: Option<String>,
    /// Remote party's address on this leg, in the same form: the To of every
    /// such request.
    pub remote_uri: Option<String>,
    /// Request-URI of the outbound INVITE — needed for CANCEL (§9.1).
    pub invite_request_uri: Option<String>,
    /// The dialog-creating INVITE this stack sent on the leg; `None` on a leg
    /// whose INVITE it received.
    pub outbound_invite: Option<OutboundInvite>,
    /// On a leg whose INVITE this stack received: the tag of the dialog this
    /// stack answers that INVITE under where no callee dialog names one —
    /// set with the leg's first dialog, moved to the dialog a 2xx confirms
    /// (RFC 3261 §8.2.6.2). `None` until the leg holds a dialog.
    pub own_tag: Option<String>,
    /// Per-service opaque extension slot (ADR-0016).
    pub ext: Option<ExtMap>,
    /// Explicit leg role (ADR-0014); read via [`crate::helpers::leg_kind`].
    pub kind: Option<LegKind>,
    /// The attempt that dialled the leg; `None` on the originator's leg and
    /// on a leg a service dials for its own use (a media leg).
    pub attempt: Option<super::attempt::AttemptId>,
    /// Whether the callee rang.
    pub ring: Ring,
    /// Whether this leg takes part in the originator's session timer (RFC
    /// 4028), fixed when the stack dials it: false on a leg whose answers never
    /// reach the originator (a media leg, one dialled after the originator's
    /// INVITE completed) or one the call withholds `timer` from. A request
    /// relayed toward a leg where it is false carries no timer. `None` on the
    /// originator's leg.
    pub in_session_timer: Option<bool>,
    /// Whether generic relay/keepalive rules own this leg; read via
    /// [`crate::helpers::is_adopted`].
    pub adopted: Option<bool>,
    /// Status of the final this stack has sent on the leg's initial inbound
    /// INVITE; `None` while that transaction is unanswered. Every TU final
    /// records itself here and so does the autonomous 487 the transaction
    /// layer sends on a CANCEL; a leg carrying one is never sent a second
    /// final (RFC 3261 §17.2.1).
    pub invite_final_sent: Option<u16>,
    /// The 2xx this stack recorded on the leg's initial inbound INVITE that the
    /// transaction layer's autonomous `487` (its answer to the caller's CANCEL)
    /// pre-empted before it reached the wire (RFC 3261 §9.2). `None` unless
    /// that race happened; the leg then states the 487 as its final and holds
    /// no confirmed dialog.
    pub answer_preempted: Option<u16>,
    /// The leg's distinct SIP messages, received and sent, under the
    /// configured cap; empty while the ring is off. Written only by
    /// [`crate::helpers::record_message`].
    pub messages: MessageRing,
    /// The session descriptions crossing the leg's dialog, kept so another
    /// author's description continues the session its peer holds (RFC 3264
    /// §8). Written only by the stack's description seam.
    pub sdp_session: LegSdpSession,
}

//! Per-leg record ([`Leg`]) and its state / disposition / role enums. Dialog
//! internals live in [`crate::model::dialog`]; the enclosing call record in
//! [`crate::model::record`].

use serde::{Deserialize, Serialize};

use super::dialog::Dialog;
use super::invite_txn::InviteTxnHandle;
use super::message_ring::MessageRing;
use super::sdp_session::LegSdpSession;
use super::services::ExtMap;

/// Remote peer endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub address: String,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegState {
    Trying,
    Early,
    Confirmed,
    Terminated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegDisposition {
    Pending,
    Bridged,
    Cancelling,
    Rejected,
}

/// Per-leg BYE disposition — how each leg was (or will be) torn down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeDisposition {
    /// We sent BYE, awaiting 200 OK or timeout (non-terminal).
    ByeSent,
    /// Remote sent BYE to us (we already replied 200).
    ByeReceived,
    /// 200 OK received for our outbound BYE.
    ByeConfirmed,
    /// BYE transaction timed out (far side unresponsive).
    ByeTimeout,
    /// CANCEL sent (pre-dialog, no BYE needed).
    Cancelled,
    /// Far side rejected INVITE (4xx/5xx/6xx, no BYE needed).
    Rejected,
    /// Leg never established (e.g. failover replaced it).
    None,
}

impl ByeDisposition {
    /// Terminal dispositions — no more SIP traffic expected for this leg.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ByeDisposition::ByeConfirmed
                | ByeDisposition::ByeReceived
                | ByeDisposition::ByeTimeout
                | ByeDisposition::Cancelled
                | ByeDisposition::Rejected
                | ByeDisposition::None
        )
    }
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

/// Per-leg state. `legId` is `"a"`, `"b-1"`, `"b-2"`, …
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    pub leg_id: String,
    pub call_id: String,
    pub from_tag: String,
    pub source: RemoteInfo,
    pub state: LegState,
    pub disposition: LegDisposition,
    /// Multiple during early state (forking); one survives after confirmed.
    pub dialogs: Vec<Dialog>,
    pub no_answer_timeout_sec: Option<i64>,
    /// How this leg was torn down. `None` while the call is active.
    pub bye_disposition: Option<ByeDisposition>,
    /// B2BUA's local address on this leg (From for outbound requests), as the
    /// leg's dialog keeps it ([`crate::StackDialog::local_uri`]).
    pub local_uri: Option<String>,
    /// Remote party's address on this leg (To for outbound requests), in the
    /// same form.
    pub remote_uri: Option<String>,
    /// Request-URI of the outbound INVITE — needed for CANCEL (§9.1).
    pub invite_request_uri: Option<String>,
    /// In-flight initial-INVITE client transaction handle on this leg.
    pub pending_invite_txn: Option<InviteTxnHandle>,
    /// Per-service opaque extension slot (ADR-0016).
    pub ext: Option<ExtMap>,
    /// Explicit leg role (ADR-0014); read via [`crate::helpers::leg_kind`].
    pub kind: Option<LegKind>,
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
    /// The leg's distinct SIP messages, received and sent, under the
    /// configured cap; empty while the ring is off. Written only by
    /// [`crate::helpers::record_message`].
    pub messages: MessageRing,
    /// The session descriptions crossing the leg's dialog, kept so another
    /// author's description continues the session its peer holds (RFC 3264
    /// §8). Written only by the stack's description seam.
    pub sdp_session: LegSdpSession,
}

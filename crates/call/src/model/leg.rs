//! Per-leg record ([`Leg`]) and its state / disposition / role enums. Dialog
//! internals live in [`crate::model::dialog`]; the enclosing call record in
//! [`crate::model::call`].

use serde::{Deserialize, Serialize};

use super::dialog::Dialog;
use super::invite_txn::InviteTxnHandle;
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
    /// B2BUA's local URI for this leg (From for outbound requests).
    pub local_uri: Option<String>,
    /// Remote party's URI for this leg (To for outbound requests).
    pub remote_uri: Option<String>,
    /// Request-URI of the outbound INVITE — needed for CANCEL (§9.1).
    pub invite_request_uri: Option<String>,
    /// In-flight initial-INVITE client transaction handle on this leg.
    pub pending_invite_txn: Option<InviteTxnHandle>,
    /// Per-service opaque extension slot (ADR-0016).
    pub ext: Option<ExtMap>,
    /// Explicit leg role (ADR-0014); read via [`crate::helpers::leg_kind`].
    pub kind: Option<LegKind>,
    /// Whether generic relay/keepalive rules own this leg; read via
    /// [`crate::helpers::is_adopted`].
    pub adopted: Option<bool>,
}

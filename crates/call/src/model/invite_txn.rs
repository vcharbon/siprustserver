//! In-flight INVITE client-transaction handle — the serializable snapshot kept
//! on a [`Leg`](crate::model::Leg) / dialog ext while its INVITE awaits a final
//! response.

use serde::{Deserialize, Serialize};

/// Host/port destination of an in-flight INVITE transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

/// Handle for an in-flight INVITE client transaction. Carries enough to rebuild
/// the CANCEL (§9.1 branch reuse) / ACK-for-2xx (§13.2.2.4 CSeq) wire form.
/// The original INVITE is stored as raw **bytes**, so the call crate stays a
/// pure leaf with no `sip-message` dependency.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteTxnHandle {
    pub branch: String,
    /// The CSeq the INVITE went out with: on a dialog, a transaction at a
    /// higher number is a later one (RFC 3261 §12.2.1.1).
    #[serde(default)]
    pub cseq: i64,
    #[serde(with = "serde_bytes")]
    pub original_invite: Vec<u8>,
    pub destination: HostPort,
}

impl InviteTxnHandle {
    /// The handle of the INVITE `original_invite`, sent at `cseq` on the
    /// client transaction `branch` toward `destination`.
    pub fn new(branch: &str, cseq: i64, original_invite: Vec<u8>, destination: HostPort) -> Self {
        InviteTxnHandle { branch: branch.into(), cseq, original_invite, destination }
    }
}

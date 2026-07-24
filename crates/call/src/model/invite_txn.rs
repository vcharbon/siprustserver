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
    #[serde(with = "serde_bytes")]
    pub original_invite: Vec<u8>,
    pub destination: HostPort,
}

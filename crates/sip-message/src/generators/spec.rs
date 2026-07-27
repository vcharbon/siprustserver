//! Input shapes the generators consume that are NOT header values: the minimal
//! dialog and INVITE-transaction views. Header inputs are the typed values
//! themselves ([`Via`](crate::header::Via),
//! [`Contact`](crate::header::Contact), …) on the `Generate*Opts`; reading
//! headers back off a parsed message is the message's own typed surface.

/// Minimal dialog shape the in-dialog generators read — deliberately
/// decoupled from any richer dialog type; callers project into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackDialog {
    pub call_id: String,
    pub local_tag: String,
    pub remote_tag: String,
    pub local_uri: String,
    pub remote_uri: String,
    pub remote_target: String,
    pub local_cseq: u32,
    pub route_set: Vec<String>,
}

/// Minimal INVITE client-transaction view the CANCEL / ACK-for-2xx
/// generators read: only the original INVITE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteClientTransactionHandle {
    pub original_invite: crate::types::SipRequest,
}

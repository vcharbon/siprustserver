//! The dialog-creating INVITE a leg sent ([`OutboundInvite`]): what each
//! dialog a response to it creates starts from (RFC 3261 §12.1.2), and how
//! its client transaction ended.

use serde::{Deserialize, Serialize};

use super::invite_txn::InviteTxnHandle;

/// The INVITE this stack sent to open a leg. Every dialog its responses
/// create carries `our_tag`, its From-tag, and starts its own sequence at
/// `cseq`, the INVITE's (§12.1.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundInvite {
    pub our_tag: String,
    pub cseq: i64,
    /// The INVITE's client transaction: what a CANCEL (§9.1) and each
    /// dialog's ACK for its 2xx (§13.2.2.4) are built from.
    pub txn: Option<InviteTxnHandle>,
    /// The final the INVITE took: the first final response's status, or 408
    /// for a transaction this stack gave up on (§8.1.3.1). `None` while it
    /// has none.
    pub final_received: Option<u16>,
    /// This stack sent a CANCEL of the INVITE (§9.1).
    pub cancel_sent: bool,
    /// The headers that CANCEL stated beyond the ones it echoes from the
    /// INVITE (a `Reason`, RFC 3326), so a node that takes the call over
    /// rebuilds the same CANCEL.
    pub cancel_headers: Vec<(String, String)>,
}

impl OutboundInvite {
    /// The INVITE `our_tag` / `cseq` names, its transaction `txn`, with no
    /// final and no CANCEL yet.
    pub fn new(our_tag: impl Into<String>, cseq: i64, txn: Option<InviteTxnHandle>) -> Self {
        OutboundInvite {
            our_tag: our_tag.into(),
            cseq,
            txn,
            final_received: None,
            cancel_sent: false,
            cancel_headers: Vec::new(),
        }
    }

    /// Record `status` as the INVITE's final; the first final stands
    /// (§17.1.1.2: the transaction takes one).
    pub fn record_final(&mut self, status: u16) {
        self.final_received.get_or_insert(status);
    }
}

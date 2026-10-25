//! An in-dialog client transaction after its request left: its retransmission,
//! and its give-up.

use super::InDialogTxn;
use crate::StepError;

impl InDialogTxn {
    /// Send the request again, byte for byte, on its next hop: the same client
    /// transaction, as its timer retransmits it over an unreliable transport
    /// until it is answered (RFC 3261 §17.1.1.2, §17.1.2.2). A transaction
    /// holding no datagram sends nothing.
    pub async fn try_retransmit(&self) -> Result<(), StepError> {
        if self.wire.is_empty() {
            return Ok(());
        }
        self.agent.try_send_wire(&self.wire, self.wire_dst).await
    }

    /// Give the transaction up as its user leaves it (a BYE ended the dialog
    /// under a re-INVITE): the endpoint still ACKs its non-2xx final hop by
    /// hop (RFC 3261 §17.1.1.3). A non-INVITE transaction owes no ACK.
    pub fn abandon(&self) {
        if let Some(invite) = &self.invite {
            self.agent.txn.abandon_invite(invite.clone(), self.wire_dst);
        }
    }
}

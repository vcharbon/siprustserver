//! The retransmission of an in-dialog client transaction's request.

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
}

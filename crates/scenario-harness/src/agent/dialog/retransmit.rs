//! An in-dialog client transaction after its request left: its give-up.

use super::InDialogTxn;

impl InDialogTxn {
    /// Give the transaction up as its user leaves it (a BYE ended the dialog
    /// under a re-INVITE): the endpoint still ACKs its non-2xx final hop by
    /// hop (RFC 3261 §17.1.1.3). A non-INVITE transaction owes no ACK.
    pub fn abandon(&self) {
        if let Some(invite) = &self.invite {
            self.agent.txn.abandon_invite(invite.clone(), self.wire_dst);
        }
    }
}

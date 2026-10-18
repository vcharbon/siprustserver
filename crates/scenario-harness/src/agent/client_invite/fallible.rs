//! The fallible sends of a [`ClientInvite`]: the ACK to its 2xx and its
//! CANCEL, a refused send surfacing as [`StepError::Transport`] where the
//! panicking forms in the parent unwrap it.

use sip_message::generators::{
    generate_ack_for_2xx, GenerateAckFor2xxOpts, InviteClientTransactionHandle,
};
use sip_message::SipMessage;

use super::super::addressing::next_hop;
use super::super::client_txn::{request_label, stated_lines, try_send_cancel};
use super::super::dialog::{Dialog, InDialogTxn};
use super::super::step::StepError;
use super::ClientInvite;

impl ClientInvite {
    /// Fallible [`ack`](Self::ack).
    pub async fn try_ack(&mut self) -> Result<Dialog, StepError> {
        self.try_ack_with_stating(None, &[]).await
    }

    /// Fallible [`ack_with_stating`](Self::ack_with_stating).
    pub async fn try_ack_with_stating(
        &mut self,
        sdp: Option<&str>,
        stated: &[(&str, &str)],
    ) -> Result<Dialog, StepError> {
        let (dialog, sent) = self.ack_reporting(sdp, stated).await;
        sent.map(|()| dialog)
    }

    /// ACK the 2xx and hand back the confirmed dialog whether or not the ACK
    /// left: a caller that must register the dialog before it yields (so its
    /// teardown can still BYE it) reads the send's outcome afterwards.
    pub async fn ack_reporting(
        &mut self,
        sdp: Option<&str>,
        stated: &[(&str, &str)],
    ) -> (Dialog, Result<(), StepError>) {
        self.honour_delayed_automatic().await;
        let handle =
            InviteClientTransactionHandle { original_invite: self.original_invite.clone() };
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            body: sdp.map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            extra_headers: stated_lines(stated),
            ..Default::default()
        };
        let ack =
            generate_ack_for_2xx(Some(&handle), &self.dialog, &opts).expect("an ACK of the 2xx");
        let dst = next_hop(&self.dialog, self.fallback_addr);
        let sent = self.agent.try_send(&SipMessage::Request(ack), dst).await;
        // If this dialog confirmed to a fork that carried its own PRACK sequence,
        // continue from THAT fork's CSeq so the post-confirm BYE/re-INVITE is
        // contiguous within the winning dialog (RFC 3261 §12.2.1.1), not a reuse
        // of a CSeq the fork already spent.
        let mut confirmed = self.dialog.clone();
        if let Some(&fork) = self.fork_cseq.get(&confirmed.remote_tag) {
            confirmed.local_cseq = confirmed.local_cseq.max(fork);
        }
        (Dialog::new(self.agent.clone(), dst, confirmed), sent)
    }

    /// Fallible [`cancel_stating`](Self::cancel_stating).
    pub async fn try_cancel_stating(
        &self,
        stated: &[(&str, &str)],
    ) -> Result<InDialogTxn, StepError> {
        try_send_cancel(&self.agent, &self.original_invite, self.wire_dst, stated).await?;
        Ok(InDialogTxn::new(
            self.agent.clone(),
            // A CANCEL transaction's finals take no ACK; the INVITE's 487 is
            // read — and auto-ACKed — via [`ClientInvite::expect`].
            None,
            self.wire_dst,
            request_label(Some("CANCEL"), &self.original_invite),
        ))
    }
}

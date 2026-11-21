//! The fallible sends of a [`ClientInvite`]: the ACK to its 2xx and its
//! CANCEL, a refused send surfacing as [`StepError::Transport`] where the
//! panicking forms (in the parent and the sibling `ack` module) unwrap it.

use sip_message::generators::{
    generate_ack_for_2xx, GenerateAckFor2xxOpts, InviteClientTransactionHandle,
};
use sip_message::{Automatic, SipMessage};

use super::super::addressing::next_hop;
use super::super::client_txn::{stated_lines, try_send_cancel, TxnKey};
use super::super::dialog::{Dialog, InDialogTxn};
use super::super::step::StepError;
use super::super::ua::media_type;
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

    /// Fallible [`ack_with_body_stating`](Self::ack_with_body_stating).
    pub async fn try_ack_with_body_stating(
        &mut self,
        content_type: &str,
        body: &[u8],
        stated: &[(&str, &str)],
    ) -> Result<Dialog, StepError> {
        let (dialog, sent) = self.ack_typed_reporting(Some(content_type), body, stated).await;
        sent.map(|()| dialog)
    }

    /// Hold the ACK-to-2xx for the declared duration (a `delayed-automatic`), if
    /// any: sleep on the (paused) clock so the peer retransmits the 2xx
    /// meanwhile (RFC 3261 §13.3.1.4). A no-op with no declaration — the ACK
    /// fires immediately, so an undeclared call is unchanged.
    async fn honour_delayed_automatic(&self) {
        if let Some(dev) = self.delayed_automatic {
            match dev.which {
                Automatic::AckTo2xx => {
                    tokio::time::sleep(std::time::Duration::from_millis(dev.delay_ms)).await;
                }
            }
        }
    }

    /// ACK the 2xx and hand back the confirmed dialog whether or not the ACK
    /// left: a caller that must register the dialog before it yields (so its
    /// teardown can still BYE it) reads the send's outcome afterwards.
    pub async fn ack_reporting(
        &mut self,
        sdp: Option<&str>,
        stated: &[(&str, &str)],
    ) -> (Dialog, Result<(), StepError>) {
        self.ack_typed_reporting(None, sdp.unwrap_or_default().as_bytes(), stated).await
    }

    /// [`ack_reporting`](Self::ack_reporting) carrying `body` typed
    /// `content_type`; with no type, a non-empty body goes as `application/sdp`.
    async fn ack_typed_reporting(
        &mut self,
        content_type: Option<&str>,
        body: &[u8],
        stated: &[(&str, &str)],
    ) -> (Dialog, Result<(), StepError>) {
        self.honour_delayed_automatic().await;
        let handle =
            InviteClientTransactionHandle { original_invite: self.original_invite.clone() };
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            body: body.to_vec(),
            content_type: content_type.map(media_type),
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
            TxnKey::cancel_of(&self.original_invite),
            // A CANCEL transaction's finals take no ACK; the INVITE's 487 is
            // read — and auto-ACKed — via [`ClientInvite::expect`].
            None,
            self.wire_dst,
        ))
    }
}

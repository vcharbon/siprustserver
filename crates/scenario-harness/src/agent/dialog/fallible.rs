//! The fallible sends of a confirmed [`Dialog`]: its BYE, any in-dialog
//! request and the ACK to a re-INVITE's 2xx, a refused send surfacing as
//! [`StepError::Transport`] where the panicking forms in the parent unwrap it.

use sip_message::generators::{generate_ack_for_2xx, GenerateAckFor2xxOpts, InDialogMethod};
use sip_message::SipMessage;

use super::super::addressing::next_hop;
use super::super::client_txn::stated_lines;
use super::super::step::StepError;
use super::super::ua::media_type;
use super::{Dialog, InDialogTxn};

impl Dialog {
    /// Fallible [`bye`](Self::bye).
    pub async fn try_bye(&mut self) -> Result<InDialogTxn, StepError> {
        self.try_request(InDialogMethod::Bye, None).await
    }

    /// Fallible [`request`](Self::request).
    pub async fn try_request(
        &mut self,
        method: InDialogMethod,
        sdp: Option<&str>,
    ) -> Result<InDialogTxn, StepError> {
        let mut req = self.send_request(method);
        if let Some(s) = sdp {
            req = req.with_sdp(s);
        }
        req.try_send().await
    }

    /// Fallible [`ack_for`](Self::ack_for).
    pub async fn try_ack_for(
        &mut self,
        invite_cseq: u32,
        sdp: Option<&str>,
    ) -> Result<(), StepError> {
        let body = sdp.map(|s| ("application/sdp", s.as_bytes().to_vec()));
        self.try_ack_carrying(invite_cseq, body, &[]).await
    }

    pub(super) async fn try_ack_carrying(
        &mut self,
        invite_cseq: u32,
        body: Option<(&str, Vec<u8>)>,
        stated: &[(&str, &str)],
    ) -> Result<(), StepError> {
        let (content_type, bytes) = match body {
            Some((ct, bytes)) => (Some(media_type(ct)), bytes),
            None => (None, Vec::new()),
        };
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            cseq: Some(invite_cseq),
            body: bytes,
            content_type,
            extra_headers: stated_lines(stated),
            ..Default::default()
        };
        let ack = generate_ack_for_2xx(None, &self.dialog, &opts);
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.try_send(&SipMessage::Request(ack), dst).await
    }
}

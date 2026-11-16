//! The [`InDialogTxn`] client transaction: an in-dialog (or out-of-dialog)
//! request awaiting its responses.

use std::future::Future;
use std::net::SocketAddr;

use sip_message::{SipRequest, SipResponse};

use super::super::client_txn::{
    try_expect_response, try_expect_response_tolerating, AckCtx, Awaited, TxnKey,
};
use super::super::step::{unwrap_step_at, StepError};
use super::super::Agent;

/// Client transaction for an in-dialog request.
///
/// For a **re-INVITE** it retains the request as sent (plus its wire
/// destination), so a 3xx–6xx final — a 488 to a rejected renegotiation, a 491
/// glare — is auto-ACKed on the re-INVITE's branch/CSeq (RFC 3261 §17.1.1.3),
/// exactly like [`ClientInvite`](crate::agent::ClientInvite)'s finals. Non-INVITE
/// transactions (BYE, INFO, CANCEL, …) retain nothing: their finals take no ACK.
pub struct InDialogTxn {
    agent: Agent,
    /// The transaction whose responses an expect reads, and its name (§17.1.3).
    txn: TxnKey,
    /// The sent request, retained ONLY when it was an (re-)INVITE — the
    /// §17.1.1.3 non-2xx auto-ACK needs its Via branch / CSeq / R-URI / Route.
    invite: Option<SipRequest>,
    /// Where the request was sent (the next hop) — the non-2xx ACK is
    /// hop-by-hop and follows the SAME path.
    wire_dst: SocketAddr,
}

impl InDialogTxn {
    pub(in crate::agent) fn new(
        agent: Agent,
        txn: TxnKey,
        invite: Option<SipRequest>,
        wire_dst: SocketAddr,
    ) -> Self {
        InDialogTxn { agent, txn, invite, wire_dst }
    }

    /// The (re-)INVITE this transaction put on the wire — `None` for a
    /// non-INVITE transaction. Reading it back is how the round's offer/answer
    /// role is known: an offer here means the answer rides the response, so the
    /// ACK to the 2xx is bodyless (RFC 3261 §13.2.2.4).
    pub fn sent_invite(&self) -> Option<&SipRequest> {
        self.invite.as_ref()
    }

    /// What an expect on this transaction awaits: a re-INVITE's carries its
    /// §17.1.1.3 auto-ACK context.
    fn awaited(&self) -> Awaited<'_> {
        match &self.invite {
            Some(invite) => {
                Awaited::Invite(AckCtx { agent: &self.agent, invite, wire_dst: self.wire_dst })
            }
            None => Awaited::Other(&self.txn),
        }
    }

    /// Give the transaction up as its user leaves it (a BYE ended the dialog
    /// under a re-INVITE): the endpoint still ACKs its non-2xx final hop by
    /// hop (RFC 3261 §17.1.1.3). A non-INVITE transaction owes no ACK.
    pub fn abandon(&self) {
        if let Some(invite) = &self.invite {
            self.agent.txn.abandon_invite(invite.clone(), self.wire_dst);
        }
    }

    /// Hop-ACK a non-2xx final to a re-INVITE THIS transaction sent (RFC 3261
    /// §17.1.1.3) — a `491 Request Pending` glare reject (§14.1), a 488. A no-op
    /// for a non-INVITE transaction or a non-matching response. The reactive
    /// actor's glare path needs this because `recv_any` surfaces the 491 as a
    /// bare response (it does NOT auto-ACK, unlike the blocking `try_expect`),
    /// yet the peer's server transaction still requires the hop ACK.
    pub async fn ack_non_2xx(&self, resp: &SipResponse) -> Result<(), StepError> {
        match self.awaited() {
            Awaited::Invite(ctx) => ctx.ack_non_2xx(resp).await,
            Awaited::Other(_) => Ok(()),
        }
    }

    /// Wait for and assert a response status. Panicking veneer over
    /// [`try_expect`](InDialogTxn::try_expect): the panic names the step and
    /// the test line that called it.
    #[track_caller]
    pub fn expect(&mut self, status: u16) -> impl Future<Output = SipResponse> + '_ {
        let site = self.agent.step("expect");
        async move { unwrap_step_at(self.try_expect(status).await, site) }
    }

    /// THE expect core for this transaction: a wrong status / timeout /
    /// unexpected request becomes a [`StepError`] (the functional lane panics
    /// on it via [`expect`](InDialogTxn::expect)).
    pub async fn try_expect(&mut self, status: u16) -> Result<SipResponse, StepError> {
        try_expect_response(&self.agent, status, &self.awaited())
            .await
            .map_err(|e| e.awaiting(|| self.txn.awaited(status)))
    }

    /// Fallible, tolerant [`try_expect`](InDialogTxn::try_expect): while waiting
    /// for the response, 200-OK any inbound request whose method is in
    /// `tolerate` (e.g. a `NOTIFY` that races ahead of the REFER's 202 on the
    /// same socket) and keep waiting — instead of mis-classifying the reorder.
    /// A real load tool faces UDP reordering, so this is the production-correct
    /// behaviour, not just a fake-fabric workaround.
    pub async fn try_expect_tolerating(
        &mut self,
        status: u16,
        tolerate: &[&str],
    ) -> Result<SipResponse, StepError> {
        try_expect_response_tolerating(&self.agent, status, tolerate, &self.awaited())
            .await
            .map_err(|e| e.awaiting(|| self.txn.awaited(status)))
    }

    /// Like [`expect`](InDialogTxn::expect), but first drains (and 200-OKs) any
    /// inbound requests whose method is in `tolerate` — the response-side analog
    /// of [`Agent::receive_tolerating`]. Under a paused clock a keepalive OPTIONS
    /// retransmit can race the awaited response on the same socket; tolerate it
    /// rather than relax the assertion (CLAUDE.md retransmit hazard).
    /// Panicking veneer over [`try_expect_tolerating`](InDialogTxn::try_expect_tolerating).
    #[track_caller]
    pub fn expect_tolerating<'a>(
        &'a mut self,
        status: u16,
        tolerate: &'a [&'a str],
    ) -> impl Future<Output = SipResponse> + 'a {
        let site = self.agent.step("expect_tolerating");
        async move { unwrap_step_at(self.try_expect_tolerating(status, tolerate).await, site) }
    }
}

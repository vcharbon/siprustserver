//! Shared client-transaction mechanics: the ONE expect core behind every
//! `expect`/`try_expect` on [`ClientInvite`](super::ClientInvite) /
//! [`ClientReinvite`](super::ClientReinvite) / [`InDialogTxn`](super::InDialogTxn),
//! the RFC 3261 §17.1.1.3 auto-ACK context, and the CANCEL build+send.

use std::net::SocketAddr;

use sip_message::generators::{
    generate_ack_for_non_2xx, generate_cancel, InviteClientTransactionHandle,
};
use sip_message::{SipMessage, SipRequest, SipResponse};

use super::server_txn::ServerTxn;
use super::step::{unwrap_step, StepError};
use super::Agent;

/// RFC 3261 §17.1.1.3 client-transaction auto-ACK context: the INVITE
/// (initial or re-INVITE) whose response is being awaited, plus the wire
/// destination it was sent to — the non-2xx ACK belongs to the INVITE
/// transaction (same branch, same CSeq number) and is hop-by-hop, so it
/// follows the SAME path. The surfaces that own an INVITE client transaction
/// ([`ClientInvite`](super::ClientInvite), [`ClientReinvite`](super::ClientReinvite),
/// [`InDialogTxn`](super::InDialogTxn) for a re-INVITE) thread this into the
/// shared expect helpers so EVERY non-2xx final they surface completes its
/// transaction the way a real txn layer does — the
/// `rfc3261.unackedInviteNon2xxFinal` audit rule gates on it.
pub(super) struct AckCtx<'a> {
    pub(super) agent: &'a Agent,
    pub(super) invite: &'a SipRequest,
    pub(super) wire_dst: SocketAddr,
}

impl AckCtx<'_> {
    /// ACK `resp` iff it is a non-2xx final belonging to THIS transaction
    /// (CSeq number + method INVITE). Everything else — provisionals, finals
    /// to a CANCEL/BYE on the same socket, a 2xx (whose ACK is a dialog-level
    /// act the scenario performs, §13.2.2.4) — is left alone.
    pub(super) async fn ack_non_2xx(&self, resp: &SipResponse) -> Result<(), StepError> {
        if resp.status < 300
            || !resp.cseq.method.as_str().eq_ignore_ascii_case("INVITE")
            || resp.cseq.seq != self.invite.cseq.seq
        {
            return Ok(());
        }
        let ack = generate_ack_for_non_2xx(self.invite, resp);
        self.agent.try_send(&SipMessage::Request(ack), self.wire_dst).await
    }
}

/// Build + send the CANCEL for a still-pending (re-)INVITE (RFC 3261 §9.1) —
/// the ONE mechanism behind [`ClientInvite::cancel`](super::ClientInvite::cancel),
/// [`ClientReinvite::cancel`](super::ClientReinvite::cancel) (which unwrap it)
/// and [`CancelHandle::cancel_best_effort`](super::CancelHandle::cancel_best_effort)
/// (which swallows the error — the call is already failing).
pub(super) async fn try_send_cancel(
    agent: &Agent,
    original_invite: &SipRequest,
    wire_dst: SocketAddr,
) -> Result<(), StepError> {
    let cancel = generate_cancel(&InviteClientTransactionHandle {
        original_invite: original_invite.clone(),
    });
    agent.try_send(&SipMessage::Request(cancel), wire_dst).await
}

/// Panicking veneer over [`try_expect_response`].
pub(super) async fn expect_response(
    agent: &Agent,
    status: u16,
    ack: Option<&AckCtx<'_>>,
) -> SipResponse {
    unwrap_step(try_expect_response(agent, status, ack).await)
}

/// Receive the next response WITHOUT asserting its status (an unsolicited `100
/// Trying` is absorbed) — the raw-await the out-of-dialog auth retry
/// ([`OutOfDialogRequest::try_send_authed`](super::OutOfDialogRequest::try_send_authed))
/// uses so a `401`/`407` keeps its challenge header. An inbound request where a
/// response is expected is an error.
pub(super) async fn recv_response_raw(agent: &Agent) -> Result<SipResponse, StepError> {
    loop {
        match agent.try_recv().await? {
            SipMessage::Response(r) if r.status == 100 => continue,
            SipMessage::Response(r) => return Ok(r),
            SipMessage::Request(r) => {
                return Err(StepError::UnexpectedKind {
                    who: agent.name.clone(),
                    detail: format!("got a {} request, expected a response", r.method),
                })
            }
        }
    }
}

/// [`try_expect_response_tolerating`] with an empty tolerate list.
pub(super) async fn try_expect_response(
    agent: &Agent,
    status: u16,
    ack: Option<&AckCtx<'_>>,
) -> Result<SipResponse, StepError> {
    try_expect_response_tolerating(agent, status, &[], ack).await
}

/// THE client-side expect core, behind every `expect`/`try_expect`
/// (`_tolerating`) on [`ClientInvite`](super::ClientInvite) /
/// [`ClientReinvite`](super::ClientReinvite) / [`InDialogTxn`](super::InDialogTxn):
/// wait for the response with status `status`, absorbing an unsolicited `100
/// Trying` (RFC 3261 §8.1.3.2 — a stateful upstream emits it before the first
/// real provisional) and 200-OKing any inbound request whose method is in
/// `tolerate` (e.g. a keepalive OPTIONS racing the awaited response on the
/// same socket). Any non-2xx final belonging to `ack`'s INVITE transaction is
/// auto-ACKed on arrival (§17.1.1.3), matching or not. A wrong status /
/// genuinely unexpected request / timeout is a [`StepError`]; the panicking
/// veneers unwrap it.
pub(super) async fn try_expect_response_tolerating(
    agent: &Agent,
    status: u16,
    tolerate: &[&str],
    ack: Option<&AckCtx<'_>>,
) -> Result<SipResponse, StepError> {
    loop {
        match agent.try_recv().await? {
            SipMessage::Response(r) if r.status == 100 && status != 100 => continue,
            SipMessage::Response(r) => {
                if let Some(ctx) = ack {
                    ctx.ack_non_2xx(&r).await?;
                }
                if r.status != status {
                    return Err(StepError::WrongStatus {
                        who: agent.name.clone(),
                        expected: status,
                        got: r.status,
                        reason: r.reason.clone(),
                    });
                }
                return Ok(r);
            }
            SipMessage::Request(r) if tolerate.iter().any(|t| r.method == *t) => {
                let mut txn = ServerTxn::from_request(agent.clone(), r);
                txn.respond(200, "OK").try_send().await?;
                continue;
            }
            SipMessage::Request(r) => {
                if agent.ack_obligation_claims(&r) {
                    continue; // txn-owned §17.1.1.3 hop ACK
                }
                let tolerating = if tolerate.is_empty() {
                    String::new()
                } else {
                    format!(" (tolerating {tolerate:?})")
                };
                return Err(StepError::UnexpectedKind {
                    who: agent.name.clone(),
                    detail: format!(
                        "got a {} request, expected a {status} response{tolerating}",
                        r.method
                    ),
                });
            }
        }
    }
}

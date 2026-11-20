//! Shared client-transaction mechanics: the ONE expect core behind every
//! `expect`/`try_expect` on [`ClientInvite`](super::ClientInvite) /
//! [`ClientReinvite`](super::ClientReinvite) / [`InDialogTxn`](super::InDialogTxn),
//! the RFC 3261 §17.1.1.3 auto-ACK context, and the CANCEL build+send.

use std::net::SocketAddr;

use sip_message::generators::{
    generate_ack_for_non_2xx, generate_cancel, InviteClientTransactionHandle,
};
use sip_message::{Method, SipHeader, SipMessage, SipRequest, SipResponse};

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
/// `unacked-invite-non-2xx-final` audit rule gates on it.
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
        if resp.status() < 300
            || resp.cseq().method() != Method::Invite
            || resp.cseq().seq() != self.invite.cseq().seq()
        {
            return Ok(());
        }
        let ack = generate_ack_for_non_2xx(self.invite, resp, &[]).expect("an ACK of the final");
        self.agent.try_send(&SipMessage::Request(ack), self.wire_dst).await
    }
}

/// A client transaction as its responses name it (RFC 3261 §17.1.3), the
/// top-Via branch and CSeq method of the request that opened it, and as a
/// timed-out expect names it ([`request_label`]).
#[derive(Debug, Clone)]
pub(crate) struct TxnKey {
    branch: String,
    method: Method,
    label: String,
}

impl TxnKey {
    /// The transaction `request` opened.
    pub(crate) fn of(request: &SipRequest) -> Self {
        let label = request_label(None, request);
        TxnKey { branch: branch_of(request), method: request.method().clone(), label }
    }

    /// The CANCEL transaction of `invite`: its branch, method CANCEL (§9.1).
    pub(crate) fn cancel_of(invite: &SipRequest) -> Self {
        let label = request_label(Some("CANCEL"), invite);
        TxnKey { branch: branch_of(invite), method: Method::Cancel, label }
    }

    /// What an expect of `status` on this transaction waits for.
    pub(super) fn awaited(&self, status: u16) -> String {
        format!("a {status} to {}", self.label)
    }
}

/// The client transaction an expect reads the responses of.
pub(super) enum Awaited<'a> {
    /// An INVITE's: its non-2xx finals are auto-ACKed (§17.1.1.3).
    Invite(AckCtx<'a>),
    /// Any other request's: its finals take no ACK.
    Other(&'a TxnKey),
}

impl Awaited<'_> {
    /// Whether `resp` answers this transaction: same top-Via branch and CSeq
    /// method as its request (§17.1.3).
    pub(super) fn owns(&self, resp: &SipResponse) -> bool {
        let (branch, method) = match self {
            Awaited::Invite(ctx) => (branch_of(ctx.invite), &Method::Invite),
            Awaited::Other(key) => (key.branch.clone(), &key.method),
        };
        resp.top_via().branch().unwrap_or_default() == branch && resp.cseq().method() == method
    }
}

/// The top-Via branch `request` went out with.
fn branch_of(request: &SipRequest) -> String {
    request.top_via().branch().unwrap_or_default().to_string()
}

/// Build + send the CANCEL for a still-pending (re-)INVITE (RFC 3261 §9.1) —
/// the ONE mechanism behind [`ClientInvite::cancel`](super::ClientInvite::cancel),
/// [`ClientReinvite::cancel`](super::ClientReinvite::cancel) (which unwrap it)
/// and [`CancelHandle::cancel_best_effort`](super::CancelHandle::cancel_best_effort)
/// (which swallows the error — the call is already failing).
///
/// `stated` are the canceller's own header lines — RFC 3326 §2 scopes `Reason`
/// to CANCEL and BYE, so this is where a caller gives up with a cause.
pub(super) async fn try_send_cancel(
    agent: &Agent,
    original_invite: &SipRequest,
    wire_dst: SocketAddr,
    stated: &[(&str, &str)],
) -> Result<(), StepError> {
    let stated = stated_lines(stated);
    let cancel = generate_cancel(
        &InviteClientTransactionHandle { original_invite: original_invite.clone() },
        &stated,
    )
    .expect("a CANCEL of the scenario's INVITE");
    agent.try_send(&SipMessage::Request(cancel), wire_dst).await
}

/// A scenario's own header lines, name and value as the test wrote them.
pub(super) fn stated_lines(stated: &[(&str, &str)]) -> Vec<SipHeader> {
    stated
        .iter()
        .map(|(name, value)| SipHeader {
            name: (*name).to_string().into(),
            value: (*value).to_string().into(),
        })
        .collect()
}

/// A sent request as an expect names it: method, CSeq and the dialog it
/// rides (Call-ID and tags). `method` overrides the request's own, for the
/// CANCEL built from an INVITE (it shares the INVITE's CSeq number and tags).
pub(super) fn request_label(method: Option<&str>, request: &SipRequest) -> String {
    let to_tag = match request.to().tag() {
        Some(t) if !t.is_empty() => format!("To-tag {t}"),
        _ => "no To-tag".to_string(),
    };
    format!(
        "{} CSeq {} (Call-ID {}, From-tag {}, {to_tag})",
        method.unwrap_or(request.method().as_str()),
        request.cseq().seq(),
        request.call_id(),
        request.from().tag().unwrap_or_default(),
    )
}

/// What an expect of `status` to `request` waits for.
pub(super) fn awaited_response(status: u16, request: &SipRequest) -> String {
    format!("a {status} to {}", request_label(None, request))
}

/// Panicking veneer over [`try_expect_response`].
pub(super) async fn expect_response(
    agent: &Agent,
    status: u16,
    awaited: &Awaited<'_>,
) -> SipResponse {
    unwrap_step(try_expect_response(agent, status, awaited).await)
}

/// Receive the next response WITHOUT asserting its status (an unsolicited `100
/// Trying` is absorbed) — the raw-await the out-of-dialog auth retry
/// ([`OutOfDialogRequest::try_send_authed`](super::OutOfDialogRequest::try_send_authed))
/// uses so a `401`/`407` keeps its challenge header. An inbound request where a
/// response is expected is an error.
pub(super) async fn recv_response_raw(agent: &Agent) -> Result<SipResponse, StepError> {
    loop {
        match agent.try_recv().await? {
            SipMessage::Response(r) if r.status() == 100 => continue,
            SipMessage::Response(r) => return Ok(r),
            SipMessage::Request(r) => {
                return Err(StepError::UnexpectedKind {
                    who: agent.name.clone(),
                    detail: format!("got a {} request, expected a response", r.method()),
                })
            }
        }
    }
}

/// [`try_expect_response_tolerating`] with an empty tolerate list.
pub(super) async fn try_expect_response(
    agent: &Agent,
    status: u16,
    awaited: &Awaited<'_>,
) -> Result<SipResponse, StepError> {
    try_expect_response_tolerating(agent, status, &[], awaited).await
}

/// THE client-side expect core, behind every `expect`/`try_expect`
/// (`_tolerating`) on [`ClientInvite`](super::ClientInvite) /
/// [`ClientReinvite`](super::ClientReinvite) / [`InDialogTxn`](super::InDialogTxn):
/// wait for the response of transaction `awaited` with status `status`. It
/// steps over a `100 Trying` (§8.1.3.2) and a repeated provisional of another
/// status (§13.2.2.1), sorts responses to other transactions with
/// [`step_over_or_set_aside`] (§17.1.3), and 200-OKs a request whose method is
/// in `tolerate`. A non-2xx final it reads to an awaited INVITE is ACKed
/// (§17.1.1.3), matching or not.
pub(super) async fn try_expect_response_tolerating(
    agent: &Agent,
    status: u16,
    tolerate: &[&str],
    awaited: &Awaited<'_>,
) -> Result<SipResponse, StepError> {
    let mut aside = Vec::new();
    let read = read_awaited(agent, status, tolerate, awaited, &mut aside).await;
    put_back(agent, aside);
    read
}

/// Sort `r`, a response to a transaction other than the one awaited: a copy
/// of one an expect read as its own (a 2xx retransmission, §13.3.1.4) or of
/// one already in `aside` is stepped over, any other joins `aside`.
pub(super) fn step_over_or_set_aside(agent: &Agent, r: SipResponse, aside: &mut Vec<SipResponse>) {
    let set_aside = aside.iter().any(|a| a.image() == r.image());
    if !set_aside && !agent.repeated_read_response(&r) {
        aside.push(r);
    }
}

/// Put the responses an expect set aside back ahead of every message not yet
/// read, in arrival order: the next receive, that transaction's own expect,
/// reads them first.
pub(super) fn put_back(agent: &Agent, aside: Vec<SipResponse>) {
    let mut held = agent.holdback.lock().unwrap();
    for msg in aside.into_iter().rev() {
        held.push_front(Ok(SipMessage::Response(msg)));
    }
}

/// The loop of [`try_expect_response_tolerating`], gathering into `aside` the
/// fresh responses to other transactions it reads.
async fn read_awaited(
    agent: &Agent,
    status: u16,
    tolerate: &[&str],
    awaited: &Awaited<'_>,
    aside: &mut Vec<SipResponse>,
) -> Result<SipResponse, StepError> {
    loop {
        let next =
            agent.try_recv().await.map_err(|e| e.awaiting(|| format!("a {status} response")));
        match next? {
            SipMessage::Response(r) if r.status() == 100 && status != 100 => continue,
            SipMessage::Response(r) if !awaited.owns(&r) => step_over_or_set_aside(agent, r, aside),
            SipMessage::Response(r) if r.status() != status && agent.repeated_provisional(&r) => {
                continue
            }
            SipMessage::Response(r) => {
                agent.txn.note_read(&r);
                if let Awaited::Invite(ctx) = awaited {
                    ctx.ack_non_2xx(&r).await?;
                }
                agent.response_dialog_check(&r)?;
                if r.status() != status {
                    return Err(StepError::WrongStatus {
                        who: agent.name.clone(),
                        expected: status,
                        got: r.status(),
                        reason: r.reason().to_string(),
                    });
                }
                return Ok(r);
            }
            SipMessage::Request(r) if tolerate.iter().any(|t| r.method() == *t) => {
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
                        r.method()
                    ),
                });
            }
        }
    }
}

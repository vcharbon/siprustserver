//! The command funnel's messages: what a [`TransactionLayer`](super::TransactionLayer)
//! method asks of the owner task, each with the channel its reply rides
//! (`layer::commands` runs them).

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::{SipMessage, SipRequest, SipResponse};
use tokio::sync::oneshot;

use crate::event::ClientTransactionHandle;
use crate::seed::{Reoffer, TxnSeed};
use call::TxnKind;

use super::super::key::ServerTxnKey;

pub(in crate::layer) enum Command {
    SendRequest {
        msg: Box<SipRequest>,
        dest: SocketAddr,
        txn_type: TxnKind,
        reply: oneshot::Sender<ClientTransactionHandle>,
    },
    SendResponse {
        msg: Box<SipResponse>,
        dest: SocketAddr,
        reply: oneshot::Sender<Bytes>,
    },
    SendRaw {
        call_id: String,
        buf: Vec<u8>,
        dest: SocketAddr,
        reply: oneshot::Sender<()>,
    },
    /// Rebuild the in-flight INVITE transactions a materialised call names
    /// (ADR-0014); replies with how many went in.
    Seed {
        call_ref: String,
        seeds: Vec<TxnSeed>,
        reply: oneshot::Sender<usize>,
    },
    /// Process a datagram the consumer already holds against the map as it now
    /// stands, after seeding.
    Reoffer {
        message: Box<SipMessage>,
        src: SocketAddr,
        reply: oneshot::Sender<Reoffer>,
    },
    CancelTxnsForCall {
        call_ref: String,
        reply: oneshot::Sender<()>,
    },
    /// Delete every transaction of `call_ref` with nothing sent; replies with
    /// how many.
    RelinquishCall {
        call_ref: String,
        reply: oneshot::Sender<usize>,
    },
    /// Delete the Accepted client transactions of `call_ref`; replies how many.
    RelinquishAccepted {
        call_ref: String,
        reply: oneshot::Sender<usize>,
    },
    /// Delete the server transaction `key` with nothing sent; replies whether
    /// it was there.
    RelinquishServer {
        key: ServerTxnKey,
        reply: oneshot::Sender<bool>,
    },
    /// Delete the server transaction `key` with nothing sent while it has
    /// sent no final and holds no CANCEL's answers; replies whether it went.
    RelinquishUnansweredServer {
        key: ServerTxnKey,
        reply: oneshot::Sender<bool>,
    },
    /// Attribute the server transaction `key`, opened attributed to no call,
    /// to `call_ref`. Nothing replies: the commands sent after it are taken
    /// after it.
    AttachServerToCall {
        key: ServerTxnKey,
        call_ref: String,
    },
    /// Count the transactions (any role/state) still attributed to `call_ref`.
    /// The B2BUA's acting-backup **self-release** (ADR-0014) polls this after
    /// serving a takeover event: when it reaches **0** the backup's served
    /// transaction(s) have met their obligation to the call (final response +
    /// ACK for an INVITE, Timer J/H for a non-INVITE, or on failure Timer F,
    /// or an INVITE's give-up plus its 64·T1 hold),
    /// so the acting-backup may shed its live takeover copy. Attribution — not
    /// `is_active()` — is deliberate: an INVITE server txn lingers in
    /// `Completed` or `Accepted` until its ACK, and shedding before the ACK
    /// would strand the ACK relay. A txn that lives on past its obligation (an
    /// INVITE server txn whose ACK it saw, in Confirmed for Timer I or
    /// Accepted for Timer L; an INVITE client txn Accepted for Timer M once
    /// its 2xx is handed up) is no longer counted.
    ActiveTxnCount {
        call_ref: String,
        reply: oneshot::Sender<usize>,
    },
    /// Delete the client transaction on `branch` with nothing sent; replies
    /// whether it was there.
    RelinquishClient {
        branch: String,
        reply: oneshot::Sender<bool>,
    },
    /// Whether a client transaction on `branch` is resident.
    HoldsClient {
        branch: String,
        reply: oneshot::Sender<bool>,
    },
    /// Forget every non-INVITE server transaction of `call_ref` that has sent
    /// no final; replies with how many.
    ForgetUnansweredOfCall {
        call_ref: String,
        reply: oneshot::Sender<usize>,
    },
    /// Answer every in-dialog INVITE server transaction of `call_ref` that
    /// has sent no final with `status` `reason`; replies with how many.
    AnswerUnansweredInvitesOfCall {
        call_ref: String,
        status: u16,
        reason: String,
        reply: oneshot::Sender<usize>,
    },
    /// Send the held answers to a CANCEL of the INVITE named by its Call-ID,
    /// From-tag and CSeq number; replies whether they left.
    ReleaseCancel {
        call_id: String,
        from_tag: String,
        cseq: u32,
        reply: oneshot::Sender<bool>,
    },
    /// Send the held ACK of the client INVITE transaction at `branch`;
    /// replies whether it left.
    ReleaseAck {
        branch: String,
        reply: oneshot::Sender<bool>,
    },
    /// Send the 100 Trying of the server INVITE transaction `key` names if it
    /// has sent nothing yet; replies whether the 100 left.
    ReleaseTrying {
        key: ServerTxnKey,
        reply: oneshot::Sender<bool>,
    },
    /// Register `call_ref` for a one-shot
    /// [`TransactionEvent::CallQuiesced`](crate::TransactionEvent::CallQuiesced)
    /// when its last transaction clears (ADR-0014 self-release). If it already
    /// has no transactions, `CallQuiesced` is emitted at once.
    WatchSelfRelease {
        call_ref: String,
        reply: oneshot::Sender<()>,
    },
}

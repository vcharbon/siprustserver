//! Public surface: [`TransactionLayer`] (the clone-cheap handle), its
//! [`TransactionConfig`], and the [`Command`] and [`Forget`] funnels to the
//! owner task. Every mutation runs on the single writer: a command round-trips
//! over an mpsc + oneshot, a forget is one-way. The owner itself lives in `layer::owner`.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use sip_message::{SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::sync::{mpsc, oneshot};

use crate::event::{ClientTransactionHandle, TransactionEvent};
use crate::metrics::{MetricsInner, TransactionMetrics};
use crate::pulse::LoopPulse;
use crate::seed::{Reoffer, TxnSeed};
use call::TxnKind;

pub use super::config::TransactionConfig;
use super::key::{ServerTxnId, ServerTxnKey};
use super::owner::Owner;
use super::run::run;

/// The command funnel's bound.
const COMMAND_CAPACITY: usize = 1024;

pub(super) enum Command {
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
    /// Register `call_ref` for a one-shot [`TransactionEvent::CallQuiesced`] when
    /// its last transaction clears (ADR-0014 self-release). If it already has no
    /// transactions, `CallQuiesced` is emitted at once.
    WatchSelfRelease {
        call_ref: String,
        reply: oneshot::Sender<()>,
    },
}

/// Forget the non-INVITE server transaction `key` names if it has sent nothing
/// yet (`Trying`) and holds the request's dialog identity; no reply. It rides
/// its own unbounded lane, which holds at most one per request the owner handed
/// up, so a stalled owner lets it grow by nothing it did not already emit.
pub(super) struct Forget {
    pub(super) key: ServerTxnKey,
    pub(super) call_id: String,
    pub(super) from_tag: String,
}

/// Handle to the running transaction layer. Clone-cheap; every method funnels
/// to the single owner task.
#[derive(Clone)]
pub struct TransactionLayer {
    cmd_tx: mpsc::Sender<Command>,
    forget_tx: mpsc::UnboundedSender<Forget>,
    metrics: TransactionMetrics,
    /// Aborts the owner task (and so drops the SIP endpoint it owns). Used to
    /// simulate a hard crash: the owner otherwise lives until every `cmd_tx` clone
    /// drops, which a surviving per-call task would keep alive — so a "crashed"
    /// node would keep answering SIP. Cheap to clone.
    owner_abort: tokio::task::AbortHandle,
    /// The owner loop's pulse: stamped from the instant a turn takes work
    /// until it waits again.
    owner_pulse: Arc<LoopPulse>,
}

impl TransactionLayer {
    /// Spawn the owner task over an already-bound endpoint. Returns the handle
    /// and the receiver end of the bounded `events` queue (the consumer — the
    /// proxy/B2BUA router — drains it). The task lives until both the returned
    /// handle (all clones) and the events receiver are dropped, or the endpoint
    /// closes.
    pub fn spawn(
        endpoint: Box<dyn UdpEndpoint>,
        parser: Arc<dyn SipParser + Send + Sync>,
        config: TransactionConfig,
    ) -> (Self, mpsc::Receiver<TransactionEvent>) {
        #[expect(clippy::disallowed_methods, reason = "event_capacity is within EVENT_CAPACITY")]
        let (events_tx, events_rx) = mpsc::channel::<TransactionEvent>(config.event_capacity());
        #[expect(clippy::disallowed_methods, reason = "COMMAND_CAPACITY is not zero")]
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(COMMAND_CAPACITY);
        let (forget_tx, forget_rx) = mpsc::unbounded_channel::<Forget>();

        let metrics_inner = Arc::new(MetricsInner::new());
        let metrics = TransactionMetrics::new(metrics_inner.clone(), events_tx.clone());

        let owner = Owner::new(parser, events_tx, metrics_inner, config);
        let owner_pulse = Arc::new(LoopPulse::new());
        let owner_abort =
            tokio::spawn(run(owner, endpoint, cmd_rx, forget_rx, owner_pulse.clone()))
                .abort_handle();

        (Self { cmd_tx, forget_tx, metrics, owner_abort, owner_pulse }, events_rx)
    }

    pub fn metrics(&self) -> &TransactionMetrics {
        &self.metrics
    }

    /// The owner loop's pulse: how long its current turn has run, `None`
    /// while it waits for work.
    pub fn owner_pulse(&self) -> &Arc<LoopPulse> {
        &self.owner_pulse
    }

    /// The owner task, for a supervisor to see it end: every handle dropped,
    /// the endpoint closed, aborted, or panicked. The layer answers nothing
    /// after it. Holding it keeps neither the task nor the endpoint alive.
    pub fn owner_task(&self) -> tokio::task::AbortHandle {
        self.owner_abort.clone()
    }

    /// Abort the owner task — and so drop the SIP endpoint it owns, silencing the
    /// wire. For hard-crash simulation (the failover harness): without it the owner
    /// outlives `B2buaCore::abort` (a surviving per-call task still holds a `cmd_tx`
    /// clone, so `cmd_rx` never closes) and the "crashed" node keeps answering SIP
    /// — 100 Trying, 200/487 to CANCEL, cached-final replays, client retransmits.
    pub fn abort_owner(&self) {
        self.owner_abort.abort();
    }

    /// Funnel one command to the owner and await its oneshot reply. Returns
    /// [`TransactionLayerClosed`] instead of panicking when the owner task is gone
    /// (endpoint closed, or `abort_owner`) — the single funnel-and-error site for
    /// every public method.
    async fn roundtrip<R>(
        &self,
        build: impl FnOnce(oneshot::Sender<R>) -> Command,
    ) -> Result<R, TransactionLayerClosed> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx.send(build(reply)).await.map_err(|_| TransactionLayerClosed)?;
        rx.await.map_err(|_| TransactionLayerClosed)
    }

    /// Send an outbound SIP request, allocating a client transaction (none
    /// for a CANCEL or an ACK) and returning its handle. An ACK of a 2xx is
    /// kept by its Accepted INVITE client transaction, which re-sends it to a
    /// repeat once the call is released. `Err` if the owner task is gone.
    pub async fn send_request(
        &self,
        msg: SipRequest,
        dest: SocketAddr,
        txn_type: TxnKind,
    ) -> Result<ClientTransactionHandle, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::SendRequest { msg: Box::new(msg), dest, txn_type, reply })
            .await
    }

    /// Send an outbound SIP response through its server transaction and
    /// return the datagram that left. The bytes are `msg.image()` verbatim
    /// unless the To-tag had to be bound to the transaction's (`bind_to_tag`),
    /// so a TU that retains an image for a repeat retains what is returned
    /// here (ADR-0032 X3). A response a server transaction here holds leaves
    /// to that transaction's destination, its request's response target
    /// (RFC 3261 §18.2.2); `dest` addresses any other.
    pub async fn send_response(
        &self,
        msg: SipResponse,
        dest: SocketAddr,
    ) -> Result<Bytes, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::SendResponse { msg: Box::new(msg), dest, reply }).await
    }

    /// Send a raw buffer directly, bypassing transaction management.
    pub async fn send_raw(
        &self,
        buf: Vec<u8>,
        dest: SocketAddr,
    ) -> Result<(), TransactionLayerClosed> {
        self.roundtrip(|reply| Command::SendRaw { buf, dest, reply }).await
    }

    /// Rebuild the in-flight INVITE transactions a call materialised from a
    /// replica names (ADR-0014), each attributed to `call_ref` — see
    /// [`TxnSeed`] for what each seed becomes. Returns how many went in; a
    /// seed whose identity already holds a transaction of its role is skipped
    /// and counted (`txn_seed_skipped`), never displaced, save an occupant
    /// with no final, which takes an Accepted seed's 2xx.
    pub async fn seed(
        &self,
        call_ref: &str,
        seeds: Vec<TxnSeed>,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::Seed { call_ref: call_ref.to_string(), seeds, reply }).await
    }

    /// Hand back a datagram this layer already emitted, to be processed
    /// against the transactions now in the map — the ones [`seed`](Self::seed)
    /// just rebuilt. A response matching a client transaction and a CANCEL
    /// matching an active INVITE server transaction run their first-arrival
    /// path; a request matching a server transaction (RFC 3261 §17.2.3) is
    /// absorbed with nothing sent, its arrival already answered (§17.2.1). All
    /// three are [`Reoffer::Matched`] (the consumer drops its copy: whatever
    /// that path emits arrives as a fresh event); anything else is
    /// [`Reoffer::Unmatched`], sent nowhere and emitted nowhere.
    pub async fn reoffer(
        &self,
        message: SipMessage,
        src: SocketAddr,
    ) -> Result<Reoffer, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::Reoffer { message: Box::new(message), src, reply }).await
    }

    /// Release every client transaction of `call_ref`, Accepted ones included:
    /// each is orphaned and closes its own obligations, the ACK of each later
    /// 2xx among them, surfacing nothing (ADR-0034). Idempotent.
    pub async fn cancel_txns_for_call(&self, call_ref: &str) -> Result<(), TransactionLayerClosed> {
        self.roundtrip(|reply| Command::CancelTxnsForCall { call_ref: call_ref.to_string(), reply })
            .await
    }

    /// Relinquish every transaction of `call_ref`, client and server, to the
    /// node that serves the call now: each is deleted with its timers, nothing
    /// sent and nothing owed, and no `CallQuiesced` follows. A message for one
    /// of them arriving afterwards matches nothing here and is handed up as a
    /// new arrival. Returns how many went. Idempotent.
    pub async fn relinquish_call(&self, call_ref: &str) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::RelinquishCall { call_ref: call_ref.to_string(), reply })
            .await
    }

    /// Delete the Accepted INVITE client transactions of `call_ref` with
    /// nothing sent, ahead of a release that does not end the call: no later
    /// 2xx draws this layer's ACK or BYE on a dialog the call keeps, and a
    /// repeat is a new arrival. Returns how many went. Idempotent.
    pub async fn relinquish_accepted_of_call(
        &self,
        call_ref: &str,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::RelinquishAccepted {
            call_ref: call_ref.to_string(),
            reply,
        })
        .await
    }

    /// Relinquish the one server transaction `key` to the node that answered
    /// its request: it is deleted with its timers, nothing sent and nothing
    /// owed. A retransmission of its request arriving afterwards matches
    /// nothing here and is handed up as a new arrival. Returns whether it was
    /// there. Idempotent.
    pub async fn relinquish_server(
        &self,
        key: ServerTxnKey,
    ) -> Result<bool, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::RelinquishServer { key, reply }).await
    }

    /// How many transactions for `call_ref` are still resident in the map (any
    /// role/state). The acting-backup self-release (ADR-0014) reads it as a
    /// defensive re-check — see `Command::ActiveTxnCount`.
    pub async fn active_txn_count_for_call(
        &self,
        call_ref: &str,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::ActiveTxnCount { call_ref: call_ref.to_string(), reply })
            .await
    }

    /// Whether this layer holds a client transaction on `branch`: the final to
    /// the request sent on it arrives here (RFC 3261 §17.1.3).
    pub async fn holds_client(&self, branch: &str) -> Result<bool, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::HoldsClient { branch: branch.to_string(), reply }).await
    }

    /// The consumer discarded the non-INVITE request whose server transaction
    /// is `key` ([`ServerTxnKey::of`] the request) before answering it: forget
    /// that transaction if it is still `Trying`, so the UAC's retransmission
    /// (RFC 3261 §17.1.2.2) is admitted afresh instead of absorbed unanswered.
    /// Another request on the same branch — another sent-by or method — is
    /// another transaction and is never forgotten, and the request's `call_id`
    /// and `from_tag` must match the transaction's; a transaction that has
    /// sent a response is kept. Never waits and is never refused: the owner
    /// takes it ahead of its commands.
    pub fn forget_unanswered(&self, key: &ServerTxnKey, call_id: &str, from_tag: &str) {
        let forget = Forget {
            key: key.clone(),
            call_id: call_id.to_string(),
            from_tag: from_tag.to_string(),
        };
        // Closed only once the owner is gone, which forgets nothing anyway.
        let _ = self.forget_tx.send(forget);
    }

    /// The consumer released `call_ref` for good: nothing will answer a request
    /// of it any more. Every non-INVITE server transaction attributed to it
    /// that has sent no final is forgotten, so the UAC's retransmission
    /// (RFC 3261 §17.1.2.2) is admitted afresh — and meets no call — instead
    /// of being absorbed until its Timer F. INVITE transactions are kept.
    /// Returns how many were forgotten.
    pub async fn forget_unanswered_of_call(
        &self,
        call_ref: &str,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::ForgetUnansweredOfCall {
            call_ref: call_ref.to_string(),
            reply,
        })
        .await
    }

    /// The consumer released `call_ref` for good while an INVITE it was
    /// handed still has no final: its 100 Trying stopped the UAC's
    /// retransmissions (RFC 3261 §17.2.1), so nothing would ever answer it.
    /// Every in-dialog INVITE server transaction attributed to the call that
    /// has sent no final is answered `status` `reason` through the
    /// transaction, under the To-tag it names, to where the request came
    /// from. An out-of-dialog INVITE naming the call starts another call and
    /// is left alone. The
    /// transaction then runs Timer G to its ACK, which it absorbs. Returns
    /// how many were answered.
    pub async fn answer_unanswered_invites_of_call(
        &self,
        call_ref: &str,
        status: u16,
        reason: &str,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::AnswerUnansweredInvitesOfCall {
            call_ref: call_ref.to_string(),
            status,
            reason: reason.to_string(),
            reply,
        })
        .await
    }

    /// Release the 100 Trying of the server INVITE transaction `invite`
    /// opened (RFC 3261 §17.2.1): it leaves now if the transaction is still
    /// `Trying` with nothing sent, and never after another response. `true`
    /// when the 100 left; `false` too when no transaction holds `invite`.
    pub async fn release_trying(
        &self,
        invite: &SipRequest,
    ) -> Result<bool, TransactionLayerClosed> {
        let Some(key) = ServerTxnId::of_request(invite).map(ServerTxnId::to_key) else {
            return Ok(false);
        };
        self.roundtrip(|reply| Command::ReleaseTrying { key, reply }).await
    }

    /// Release the ACK of the non-2xx final the client INVITE transaction at
    /// `branch` took ([`TransactionConfig::ack_bound_ms`](crate::TransactionConfig)):
    /// it leaves now if it is still held. `true` when it left; `false` when
    /// none is held there.
    pub async fn release_ack(&self, branch: &str) -> Result<bool, TransactionLayerClosed> {
        let branch = branch.to_string();
        self.roundtrip(|reply| Command::ReleaseAck { branch, reply }).await
    }

    /// Ask to be notified (via [`TransactionEvent::CallQuiesced`]) when the last
    /// transaction for `call_ref` clears — the push signal the B2BUA acting-backup
    /// self-release (ADR-0014) arms when it takes a dialog over. Idempotent.
    pub async fn watch_self_release(&self, call_ref: &str) -> Result<(), TransactionLayerClosed> {
        self.roundtrip(|reply| Command::WatchSelfRelease { call_ref: call_ref.to_string(), reply })
            .await
    }
}

/// Returned by every [`TransactionLayer`] method when the owner task is no longer
/// running (the endpoint closed, or [`TransactionLayer::abort_owner`] was called).
/// Lets callers wind down gracefully instead of panicking on a dead owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionLayerClosed;

impl std::fmt::Display for TransactionLayerClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transaction layer owner task is no longer running")
    }
}

impl std::error::Error for TransactionLayerClosed {}

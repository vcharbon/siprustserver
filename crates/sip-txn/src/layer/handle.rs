//! Public surface: [`TransactionLayer`] (the clone-cheap handle), its
//! [`TransactionConfig`], and the [`Command`] funnel to the owner task. Every
//! method round-trips over an mpsc + oneshot, so all mutations run on the
//! single writer. The owner itself lives in `layer::owner`.

use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::{SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::sync::{mpsc, oneshot};

use crate::event::{ClientTransactionHandle, TransactionEvent, TxnKind};
use crate::metrics::{MetricsInner, TransactionMetrics};
use crate::rng::IdGen;

use super::owner::{run, Owner};

/// Tunables for the transaction layer.
pub struct TransactionConfig {
    /// The network layer's recv-queue bound. The output event queue is sized
    /// `max(64, udp_queue_max * 4)`.
    pub udp_queue_max: usize,
    /// Identifier seam (Via branch / To-tag generation).
    pub id_gen: Arc<IdGen>,
}

impl Default for TransactionConfig {
    fn default() -> Self {
        Self {
            udp_queue_max: 256,
            id_gen: Arc::new(IdGen::from_entropy()),
        }
    }
}

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
        reply: oneshot::Sender<()>,
    },
    SendRaw {
        buf: Vec<u8>,
        dest: SocketAddr,
        reply: oneshot::Sender<()>,
    },
    CancelTxnsForCall {
        call_ref: String,
        reply: oneshot::Sender<()>,
    },
    /// Count the transactions (any role/state) still resident in the map for
    /// `call_ref`. The B2BUA's acting-backup **self-release** (ADR-0014) polls
    /// this after serving a takeover event: when it reaches **0** the backup's
    /// served transaction(s) have fully cleaned up (final response + ACK for an
    /// INVITE, Timer J/H for a non-INVITE, or Timer B/F on failure), so the
    /// acting-backup may shed its live takeover copy. "Resident in the map" — not
    /// merely `is_active()` — is deliberate: an INVITE server txn lingers in
    /// `Completed` until its ACK, and shedding before the ACK would strand the
    /// ACK relay.
    ActiveTxnCount {
        call_ref: String,
        reply: oneshot::Sender<usize>,
    },
    /// Register `call_ref` for a one-shot [`TransactionEvent::CallQuiesced`] when
    /// its last transaction clears (ADR-0014 self-release). If it already has no
    /// transactions, `CallQuiesced` is emitted at once.
    WatchSelfRelease {
        call_ref: String,
        reply: oneshot::Sender<()>,
    },
}

/// Handle to the running transaction layer. Clone-cheap; every method funnels
/// to the single owner task.
#[derive(Clone)]
pub struct TransactionLayer {
    cmd_tx: mpsc::Sender<Command>,
    metrics: TransactionMetrics,
    /// Aborts the owner task (and so drops the SIP endpoint it owns). Used to
    /// simulate a hard crash: the owner otherwise lives until every `cmd_tx` clone
    /// drops, which a surviving per-call task would keep alive — so a "crashed"
    /// node would keep answering SIP. Cheap to clone.
    owner_abort: tokio::task::AbortHandle,
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
        let event_capacity = std::cmp::max(64, config.udp_queue_max * 4);
        let (events_tx, events_rx) = mpsc::channel::<TransactionEvent>(event_capacity);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(1024);

        let metrics_inner = Arc::new(MetricsInner::new());
        let metrics = TransactionMetrics::new(metrics_inner.clone(), events_tx.clone());

        let owner = Owner::new(parser, events_tx, metrics_inner, config.id_gen);
        let owner_abort = tokio::spawn(run(owner, endpoint, cmd_rx)).abort_handle();

        (Self { cmd_tx, metrics, owner_abort }, events_rx)
    }

    pub fn metrics(&self) -> &TransactionMetrics {
        &self.metrics
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
        self.cmd_tx
            .send(build(reply))
            .await
            .map_err(|_| TransactionLayerClosed)?;
        rx.await.map_err(|_| TransactionLayerClosed)
    }

    /// Send an outbound SIP request, allocating a client transaction and
    /// returning its handle. `Err` if the owner task is gone (see [`roundtrip`]).
    pub async fn send_request(
        &self,
        msg: SipRequest,
        dest: SocketAddr,
        txn_type: TxnKind,
    ) -> Result<ClientTransactionHandle, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::SendRequest {
            msg: Box::new(msg),
            dest,
            txn_type,
            reply,
        })
        .await
    }

    /// Send an outbound SIP response through its server transaction.
    pub async fn send_response(
        &self,
        msg: SipResponse,
        dest: SocketAddr,
    ) -> Result<(), TransactionLayerClosed> {
        self.roundtrip(|reply| Command::SendResponse {
            msg: Box::new(msg),
            dest,
            reply,
        })
        .await
    }

    /// Send a raw buffer directly, bypassing transaction management.
    pub async fn send_raw(&self, buf: Vec<u8>, dest: SocketAddr) -> Result<(), TransactionLayerClosed> {
        self.roundtrip(|reply| Command::SendRaw { buf, dest, reply }).await
    }

    /// Cancel every client transaction whose `call_ref` matches — the
    /// call-eviction teardown (so Timer B/F can't fire against a vanished
    /// call). Idempotent.
    pub async fn cancel_txns_for_call(&self, call_ref: &str) -> Result<(), TransactionLayerClosed> {
        self.roundtrip(|reply| Command::CancelTxnsForCall {
            call_ref: call_ref.to_string(),
            reply,
        })
        .await
    }

    /// How many transactions for `call_ref` are still resident in the map (any
    /// role/state). The acting-backup self-release (ADR-0014) reads it as a
    /// defensive re-check — see [`Command::ActiveTxnCount`].
    pub async fn active_txn_count_for_call(
        &self,
        call_ref: &str,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::ActiveTxnCount {
            call_ref: call_ref.to_string(),
            reply,
        })
        .await
    }

    /// Ask to be notified (via [`TransactionEvent::CallQuiesced`]) when the last
    /// transaction for `call_ref` clears — the push signal the B2BUA acting-backup
    /// self-release (ADR-0014) arms when it takes a dialog over. Idempotent.
    pub async fn watch_self_release(&self, call_ref: &str) -> Result<(), TransactionLayerClosed> {
        self.roundtrip(|reply| Command::WatchSelfRelease {
            call_ref: call_ref.to_string(),
            reply,
        })
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

//! Public surface: [`TransactionLayer`] (the clone-cheap handle), its
//! [`TransactionConfig`], and the [`Command`] funnel to the owner task. Every
//! method round-trips over an mpsc + oneshot, so all mutations run on the
//! single writer. The owner itself lives in `layer::owner`.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use sip_message::{SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::sync::{mpsc, oneshot};

use crate::event::{ClientTransactionHandle, TransactionEvent, TxnKind};
use crate::metrics::{MetricsInner, TransactionMetrics};
use crate::rng::IdGen;
use crate::seed::{Reoffer, TxnSeed};

use super::owner::{run, Owner};

/// Tunables for the transaction layer.
pub struct TransactionConfig {
    /// The network layer's recv-queue bound. The output event queue is sized
    /// `max(64, udp_queue_max * 4)`.
    pub udp_queue_max: usize,
    /// Identifier seam (Via branch / To-tag generation).
    pub id_gen: Arc<IdGen>,
    /// The INVITE transaction bound once a provisional has arrived, ms — the
    /// client txn's give-up timer for an INVITE in Proceeding (initial or
    /// in-dialog; before any response it is on Timer B) AND the server-side
    /// sweep age for a pre-final INVITE derive from this one value, so both
    /// halves of a call admit the same ring window. Default
    /// [`INVITE_INITIAL_TIMEOUT`](crate::timers::INVITE_INITIAL_TIMEOUT)
    /// (158 s); the consumer validates its own range and MUST keep every
    /// app-level setup deadline strictly below it, or the txn layer CANCELs
    /// the callee before the app gives up.
    pub invite_initial_timeout_ms: u64,
    /// The bound on an initial INVITE's wait for a response of ANY kind, ms —
    /// the client txn's give-up while an out-of-dialog INVITE (no To-tag) sits
    /// in `Calling`, and the bound its Timer A ladder is armed under, so no
    /// rung lands past the give-up. Default [`TIMER_B`](crate::timers::TIMER_B)
    /// (32 s), the RFC 3261 §17.1.1.2 value; a deployment may tighten it
    /// (telephony policy: a hop drawing nothing is dead, and rerouting must not
    /// wait 64·T1), never widen it — the effective bound is
    /// `min(this, invite_initial_timeout_ms)`, and the first provisional still
    /// swaps in `invite_initial_timeout_ms`. An in-dialog INVITE and a
    /// non-INVITE never read it: they keep Timer B / Timer F.
    pub invite_first_response_timeout_ms: u64,
    /// The held-CANCEL policy for a response-less INVITE client txn
    /// (RFC 3261 §9.1 / ADR-0028). `Some(ms)` — the default,
    /// [`CANCEL_HOLD_GRACE`](crate::timers::CANCEL_HOLD_GRACE) (1 s) — holds
    /// the CANCEL for the branch's first provisional at most `ms`, then sends
    /// it regardless: every emitted CANCEL reaches the wire; the grace is a
    /// courtesy window, never a veto. `None` is the strict §9.1 wait: the
    /// CANCEL is held until a provisional arrives and silently dropped if the
    /// txn dies first — a callee that never sends one is never CANCELed
    /// (ADR-0028 documents when that trade is acceptable).
    pub cancel_hold_grace_ms: Option<u64>,
    /// Whether a response the TU hands over under a To-tag other than the
    /// bound one fails a debug build loudly (`debug_assert!`) besides being
    /// re-rendered. The wire is corrected either way; `true` — the default —
    /// makes the defect a test failure, and a test that exercises the
    /// correction itself turns it off.
    pub strict_to_tag: bool,
}

impl Default for TransactionConfig {
    fn default() -> Self {
        Self {
            udp_queue_max: 256,
            id_gen: Arc::new(IdGen::from_entropy()),
            invite_initial_timeout_ms: crate::timers::INVITE_INITIAL_TIMEOUT,
            invite_first_response_timeout_ms: crate::timers::TIMER_B,
            cancel_hold_grace_ms: Some(crate::timers::CANCEL_HOLD_GRACE),
            strict_to_tag: true,
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
    /// Count the transactions (any role/state) still attributed to `call_ref`.
    /// The B2BUA's acting-backup **self-release** (ADR-0014) polls this after
    /// serving a takeover event: when it reaches **0** the backup's served
    /// transaction(s) have met their obligation to the call (final response +
    /// ACK for an INVITE, Timer J/H for a non-INVITE, or Timer B/F on failure),
    /// so the acting-backup may shed its live takeover copy. Attribution — not
    /// `is_active()` — is deliberate: an INVITE server txn lingers in
    /// `Completed` until its ACK, and shedding before the ACK would strand the
    /// ACK relay. A txn that lives on past its obligation (a non-2xx INVITE
    /// server txn in Confirmed for Timer I) is detached and no longer counted.
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

        let owner = Owner::new(
            parser,
            events_tx,
            metrics_inner,
            config.id_gen,
            config.invite_initial_timeout_ms,
            config.invite_first_response_timeout_ms,
            config.cancel_hold_grace_ms,
            config.strict_to_tag,
        );
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
        self.cmd_tx.send(build(reply)).await.map_err(|_| TransactionLayerClosed)?;
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
        self.roundtrip(|reply| Command::SendRequest { msg: Box::new(msg), dest, txn_type, reply })
            .await
    }

    /// Send an outbound SIP response through its server transaction and
    /// return the datagram that left. The bytes are `msg.image()` verbatim
    /// unless the To-tag had to be bound to the transaction's (`bind_to_tag`),
    /// so a TU that retains an image for a repeat retains what is returned
    /// here (ADR-0029 X3).
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
    /// replica names (ADR-0014), each as a `Proceeding` transaction attributed
    /// to `call_ref` — see [`TxnSeed`] for what each seed becomes. Returns how
    /// many went in; a seed whose branch already holds a transaction is skipped
    /// and counted (`txn_seed_skipped`), never displaced.
    pub async fn seed(
        &self,
        call_ref: &str,
        seeds: Vec<TxnSeed>,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::Seed { call_ref: call_ref.to_string(), seeds, reply }).await
    }

    /// Hand back a datagram this layer already emitted, to be processed
    /// against the transactions now in the map — the ones [`seed`](Self::seed)
    /// just rebuilt. A response matching a client transaction, a CANCEL
    /// matching an active INVITE server transaction and a request whose branch
    /// holds a server transaction run their first-arrival path and are
    /// [`Reoffer::Matched`] (the consumer drops its copy: whatever that path
    /// emits arrives as a fresh event); anything else is
    /// [`Reoffer::Unmatched`], sent nowhere and emitted nowhere.
    pub async fn reoffer(
        &self,
        message: SipMessage,
        src: SocketAddr,
    ) -> Result<Reoffer, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::Reoffer { message: Box::new(message), src, reply }).await
    }

    /// Cancel every client transaction whose `call_ref` matches — the
    /// call-eviction teardown (so Timer B/F can't fire against a vanished
    /// call). Idempotent.
    pub async fn cancel_txns_for_call(&self, call_ref: &str) -> Result<(), TransactionLayerClosed> {
        self.roundtrip(|reply| Command::CancelTxnsForCall { call_ref: call_ref.to_string(), reply })
            .await
    }

    /// How many transactions for `call_ref` are still resident in the map (any
    /// role/state). The acting-backup self-release (ADR-0014) reads it as a
    /// defensive re-check — see [`Command::ActiveTxnCount`].
    pub async fn active_txn_count_for_call(
        &self,
        call_ref: &str,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::ActiveTxnCount { call_ref: call_ref.to_string(), reply })
            .await
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

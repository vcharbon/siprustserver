//! The transaction layer actor — port of `TransactionLayer.ts`.
//!
//! ## Shape (see docs/adr/0007)
//!
//! The source is a single Effect fiber draining one inbound stream over a
//! lock-free `MutableHashMap`, with a pair of timer fibers per client txn and
//! a cleanup fiber per completed server txn. On multi-threaded tokio that maps
//! to **one owner task** ("the actor") that:
//!   - owns `txns` (the transaction map) — no locks, single writer;
//!   - owns one [`DelayQueue`] holding *every* pending SIP timer keyed by
//!     branch — flat memory at 50K calls instead of ~100K timer tasks;
//!   - `select!`s over (1) the external send API, (2) inbound packets it parses
//!     inline, (3) the next timer expiry, (4) the safety-net sweep.
//!
//! The send API (`send_request`/`send_response`/`send_raw`/
//! `cancel_txns_for_call`) funnels commands to the owner over an mpsc and
//! awaits a oneshot reply, so every mutation runs on the one writer — the
//! ADR-0005 single-writer seam, preserved without a per-call dispatcher (which
//! is a B2BUA-only concern; this layer is shared with the proxy).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::generators::{generate_ack_for_non_2xx, generate_response, GenerateResponseOpts};
use sip_message::message_helpers::decode_param;
use sip_message::{serialize, ParamValue, SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::sync::{mpsc, oneshot};
use tokio_util::time::{delay_queue::Key, DelayQueue};

use crate::event::{ClientTransactionHandle, EventQueueDropReason, TransactionEvent, TxnKind};
use crate::metrics::{MetricsInner, TransactionMetrics};
use crate::rng::IdGen;
use crate::timers::{
    ms, INVITE_INITIAL_TIMEOUT, T1, T2, TIMER_B, TIMER_F, TIMER_H, TIMER_J, TXN_MAX_AGE,
    TXN_SWEEP_INTERVAL,
};

// ---------------------------------------------------------------------------
// Internal transaction state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxnRole {
    Client,
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxnState {
    Trying,
    Proceeding,
    Completed,
    /// Present for fidelity with the source FSM; in this port a final
    /// response deletes the transaction outright rather than parking it in a
    /// `terminated` state, so the variant is currently never set.
    #[allow(dead_code)]
    Terminated,
}

impl TxnState {
    /// Still timing/retransmitting — Timer B/F and retransmits act only here.
    fn is_active(self) -> bool {
        matches!(self, TxnState::Trying | TxnState::Proceeding)
    }
}

/// Which DelayQueue entry a fired timer corresponds to (keyed by branch).
#[derive(Debug, Clone)]
enum Timer {
    /// Timer A (INVITE) / Timer E (non-INVITE) — client retransmit.
    ClientRetransmit(String),
    /// Timer B (INVITE) / Timer F (non-INVITE) — client transaction timeout.
    ClientTimeout(String),
    /// Timer H (INVITE) / Timer J (non-INVITE) / Timer-H-487 — server cleanup.
    ServerCleanup(String),
    /// Re-offer CRITICAL events (Timeout / Cancelled / CallQuiesced / a consumed
    /// non-2xx final / an inbound INVITE whose 100 silenced the UAC) that a full
    /// events queue deferred — never dropped. At most ONE in flight
    /// (`event_retry_armed`); its `Key` is never stored, so the CLAUDE.md
    /// stale-`Key` aliasing hazard cannot arise.
    EventRetry,
}

/// Retry cadence for deferred critical-event delivery. Short — the router drains
/// the events queue continuously, so capacity returns within a few polls; the
/// timer exists only so an otherwise-idle owner (no packets, no commands, no
/// txn timers left) still delivers the backlog.
const EVENT_RETRY_MS: u64 = 100;

struct Transaction {
    branch: String,
    role: TxnRole,
    kind: TxnKind,
    /// Method that created the txn. Kept for the per-(method,role,state)
    /// `transactionBreakdown` gauge, whose port is deferred (not asserted by
    /// the ported tests); read once that gauge lands.
    #[allow(dead_code)]
    method: String,
    call_id: String,
    from_tag: String,
    original_request: Option<SipRequest>,
    last_response: Option<Vec<u8>>,
    last_response_status: Option<u16>,
    call_ref: Option<String>,
    leg_id: Option<String>,
    state: TxnState,
    destination: Option<SocketAddr>,
    created_at: tokio::time::Instant,
    /// UAS To-tag pinned on the first >100 response (RFC 3261 §17.2.1).
    uas_to_tag: Option<String>,
    // DelayQueue keys so we can cancel a txn's timers in O(1).
    retransmit_key: Option<Key>,
    timeout_key: Option<Key>,
    cleanup_key: Option<Key>,
    // Retransmit progression (mirrors the source's `interval`/`elapsed` locals).
    retransmit_buf: Option<Vec<u8>>,
    retransmit_interval_ms: u64,
    retransmit_elapsed_ms: u64,
    retransmit_max_ms: u64,
}

// ---------------------------------------------------------------------------
// Public handle + configuration
// ---------------------------------------------------------------------------

/// Tunables for the transaction layer.
pub struct TransactionConfig {
    /// The network layer's recv-queue bound. The output event queue is sized
    /// `max(64, udp_queue_max * 4)` — the source's `Math.max(64, udpQueueMax*4)`.
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

enum Command {
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

        let owner = Owner {
            txns: HashMap::new(),
            timers: DelayQueue::new(),
            parser,
            events_tx,
            metrics: metrics_inner,
            id_gen: config.id_gen,
            self_release_watch: HashSet::new(),
            txn_index: HashMap::new(),
            deferred_events: VecDeque::new(),
            event_retry_armed: false,
            pending_quiesce: Vec::new(),
        };

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

    /// Send an outbound SIP request, allocating a client transaction and
    /// returning its handle.
    pub async fn send_request(
        &self,
        msg: SipRequest,
        dest: SocketAddr,
        txn_type: TxnKind,
    ) -> ClientTransactionHandle {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::SendRequest {
                msg: Box::new(msg),
                dest,
                txn_type,
                reply,
            })
            .await
            .expect("transaction layer owner task dropped");
        rx.await.expect("transaction layer owner task dropped")
    }

    /// Send an outbound SIP response through its server transaction.
    pub async fn send_response(&self, msg: SipResponse, dest: SocketAddr) {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::SendResponse {
                msg: Box::new(msg),
                dest,
                reply,
            })
            .await
            .expect("transaction layer owner task dropped");
        rx.await.expect("transaction layer owner task dropped");
    }

    /// Send a raw buffer directly, bypassing transaction management.
    pub async fn send_raw(&self, buf: Vec<u8>, dest: SocketAddr) {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::SendRaw { buf, dest, reply })
            .await
            .expect("transaction layer owner task dropped");
        rx.await.expect("transaction layer owner task dropped");
    }

    /// Cancel every client transaction whose `call_ref` matches — the
    /// call-eviction teardown (so Timer B/F can't fire against a vanished
    /// call). Idempotent.
    pub async fn cancel_txns_for_call(&self, call_ref: &str) {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::CancelTxnsForCall {
                call_ref: call_ref.to_string(),
                reply,
            })
            .await
            .expect("transaction layer owner task dropped");
        rx.await.expect("transaction layer owner task dropped");
    }

    /// How many transactions for `call_ref` are still resident in the map (any
    /// role/state). The acting-backup self-release (ADR-0014) reads it as a
    /// defensive re-check — see [`Command::ActiveTxnCount`].
    pub async fn active_txn_count_for_call(&self, call_ref: &str) -> usize {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::ActiveTxnCount {
                call_ref: call_ref.to_string(),
                reply,
            })
            .await
            .expect("transaction layer owner task dropped");
        rx.await.expect("transaction layer owner task dropped")
    }

    /// Ask to be notified (via [`TransactionEvent::CallQuiesced`]) when the last
    /// transaction for `call_ref` clears — the push signal the B2BUA acting-backup
    /// self-release (ADR-0014) arms when it takes a dialog over. Idempotent.
    pub async fn watch_self_release(&self, call_ref: &str) {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::WatchSelfRelease {
                call_ref: call_ref.to_string(),
                reply,
            })
            .await
            .expect("transaction layer owner task dropped");
        rx.await.expect("transaction layer owner task dropped");
    }
}

// ---------------------------------------------------------------------------
// The owner task
// ---------------------------------------------------------------------------

struct Owner {
    txns: HashMap<String, Transaction>,
    timers: DelayQueue<Timer>,
    parser: Arc<dyn SipParser + Send + Sync>,
    events_tx: mpsc::Sender<TransactionEvent>,
    metrics: Arc<MetricsInner>,
    id_gen: Arc<IdGen>,
    /// call_refs the consumer asked to be told about when their **last**
    /// transaction clears (ADR-0014 acting-backup self-release). On the
    /// last-`delete_txn` for a watched call we capture
    /// [`TransactionEvent::CallQuiesced`] onto the lossless critical path and drop
    /// the watch. Bounded — one entry per live takeover copy, removed when it fires.
    self_release_watch: HashSet<String>,
    /// `call_ref → its set of live branches`, kept in **lockstep** with `txns`
    /// (every `set_txn`/`delete_txn` updates both; the entry is dropped when the set
    /// empties). Makes the acting-backup self-release machinery — `has_txns_for`,
    /// `active_txn_count_for_call`, the last-`delete_txn` check — AND the
    /// per-call-eviction `do_cancel_txns_for_call` O(k-branches) instead of an
    /// O(total_txns) scan of the branch-keyed map, which on a backup serving many
    /// failed-over dialogs ran per teardown under endurance load. Same single-writer
    /// discipline CLAUDE.md prescribes for the timer driver's `active`/queue
    /// lockstep: never let this drift from `txns`.
    txn_index: HashMap<String, HashSet<String>>,
    /// CRITICAL events a full events queue deferred, in FIFO order — re-offered on
    /// the [`Timer::EventRetry`] tick, NEVER dropped. These are one-shot signals
    /// whose protocol-level redelivery the layer already consumed (a deleted txn's
    /// Timeout, an answered CANCEL's Cancelled, a takeover copy's only CallQuiesced,
    /// an auto-ACKed non-2xx final, an inbound INVITE whose 100 silenced the UAC);
    /// the old drop-newest `emit` lost them exactly under the post-failover/overload
    /// burst that produces them. Bounded by live txns + watches (same order as the
    /// `txns` map), so this is not a new unbounded buffer.
    deferred_events: VecDeque<TransactionEvent>,
    /// Whether a [`Timer::EventRetry`] is already in the wheel (at most one).
    event_retry_armed: bool,
    /// call_refs whose last txn cleared THIS turn but whose `CallQuiesced` must be
    /// emitted only AFTER the turn's protocol events (the ACK/Timeout that drove the
    /// delete) — else the router self-releases the takeover copy ahead of the very
    /// event it was about, orphaning it. Drained by `flush_pending_quiesce` at the
    /// end of every owner turn.
    pending_quiesce: Vec<String>,
}

/// The next expired timer. Only ever awaited while `q` is non-empty — an empty
/// `DelayQueue` resolves `poll_expired` to `Ready(None)`, which would busy-spin
/// `select!`, so the call site guards with `if !timers.is_empty()`.
async fn next_expired(q: &mut DelayQueue<Timer>) -> Timer {
    std::future::poll_fn(|cx| q.poll_expired(cx))
        .await
        .expect("guarded by !is_empty()")
        .into_inner()
}

async fn run(mut owner: Owner, endpoint: Box<dyn UdpEndpoint>, mut cmd_rx: mpsc::Receiver<Command>) {
    let endpoint: &dyn UdpEndpoint = endpoint.as_ref();
    let mut sweep = tokio::time::interval(ms(TXN_SWEEP_INTERVAL));
    // Coalesce missed ticks: after any owner stall the catch-up must be a single
    // pass, not a burst of back-to-back full-map scans (default `Burst`).
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first `interval` tick is immediate; skip it (the source's sweep
    // fiber sleeps before its first pass).
    sweep.tick().await;

    loop {
        // `biased` polls the arms top-to-bottom and the INBOUND PACKET arm is LAST
        // on purpose: a sustained packet flood keeps `endpoint.recv()` ready every
        // iteration, so if it were polled first it would starve the timer wheel and
        // the safety-net sweep indefinitely — retransmits/timeouts/cleanups would
        // stall and the txns map + DelayQueue would grow unbounded exactly under
        // the overload the sweep exists to bound. Polling commands, due timers, and
        // the sweep ahead of new packets guarantees the internal machinery always
        // makes progress; the flood source (packets) can only be drained once
        // nothing else is pending. (cmd/timer can't themselves flood without
        // packets — the router only issues commands in response to events.)
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => match cmd {
                Some(c) => owner.handle_command(endpoint, c).await,
                None => break, // all handles dropped → shut down
            },
            timer = next_expired(&mut owner.timers), if !owner.timers.is_empty() => {
                owner.fire_timer(endpoint, timer).await;
            }
            _ = sweep.tick() => owner.sweep(),
            packet = endpoint.recv() => match packet {
                Some(p) => owner.handle_packet(endpoint, p).await,
                None => break, // endpoint closed
            },
        }
        // End-of-turn: emit deferred CallQuiesced notices AFTER this turn's
        // protocol events, so the router never self-releases a takeover copy ahead
        // of the ACK/Timeout that cleared the call's last txn (ADR-0014 ordering).
        owner.flush_pending_quiesce();
    }
}

impl Owner {
    // ── map bookkeeping (keeps the active-txn gauge == map size) ────────────

    fn set_txn(&mut self, txn: Transaction) {
        let new_call_ref = txn.call_ref.clone();
        let branch = txn.branch.clone();
        // A re-insert of the same branch (rare) must not double-count: drop the
        // displaced txn's contribution before adding the new one.
        if let Some(old) = self.txns.insert(branch.clone(), txn) {
            // The displaced txn's queue entries are keyed by the SAME branch string
            // the replacement now owns; left in the wheel they would fire against
            // the new txn (spurious retransmit/timeout/cleanup) and their Keys would
            // alias once their slots are reused. Physically remove them now —
            // cancel and queue membership move together (CLAUDE.md). (old.branch ==
            // branch — the same map key.)
            self.cancel_timer(old.retransmit_key);
            self.cancel_timer(old.timeout_key);
            self.cancel_timer(old.cleanup_key);
            self.untrack_call_ref(&old.call_ref, &branch);
        }
        self.track_call_ref(&new_call_ref, &branch);
        self.sync_active();
    }

    /// Add `branch` to `call_ref`'s live-branch set, in lockstep with a `txns`
    /// insert. A txn with no `call_ref` (out-of-dialog initial INVITE / OPTIONS) is
    /// not indexed.
    fn track_call_ref(&mut self, call_ref: &Option<String>, branch: &str) {
        if let Some(cr) = call_ref {
            self.txn_index
                .entry(cr.clone())
                .or_default()
                .insert(branch.to_string());
        }
    }

    /// Remove `branch` from `call_ref`'s live-branch set, in lockstep with a `txns`
    /// remove; the entry is dropped when the set empties so `has_txns_for` is a
    /// plain `contains_key`.
    fn untrack_call_ref(&mut self, call_ref: &Option<String>, branch: &str) {
        if let Some(cr) = call_ref {
            if let Some(set) = self.txn_index.get_mut(cr) {
                set.remove(branch);
                if set.is_empty() {
                    self.txn_index.remove(cr);
                }
            }
        }
    }

    fn delete_txn(&mut self, branch: &str) -> bool {
        match self.txns.remove(branch) {
            Some(t) => {
                self.cancel_timer(t.retransmit_key);
                self.cancel_timer(t.timeout_key);
                self.cancel_timer(t.cleanup_key);
                self.untrack_call_ref(&t.call_ref, branch);
                self.sync_active();
                // ADR-0014 self-release: if this was the LAST transaction for a
                // watched call, the consumer must hear CallQuiesced so it can shed
                // its acting-backup takeover copy — but only AFTER this turn's
                // protocol event (the ACK/Timeout that drove the delete), so defer
                // it to `flush_pending_quiesce`. (For a 2xx INVITE the server txn
                // lingers in `Completed` until Timer H — the ACK reuses a different
                // branch — so this naturally fires at Timer H, after the ACK relay.)
                if let Some(cr) = t.call_ref {
                    if self.self_release_watch.contains(&cr) && !self.has_txns_for(&cr) {
                        self.pending_quiesce.push(cr);
                    }
                }
                true
            }
            None => false,
        }
    }

    fn cancel_timer(&mut self, key: Option<Key>) {
        if let Some(k) = key {
            self.timers.try_remove(&k);
        }
    }

    fn sync_active(&self) {
        self.metrics
            .active_transactions
            .store(self.txns.len(), std::sync::atomic::Ordering::Relaxed);
    }

    // ── output ──────────────────────────────────────────────────────────────

    async fn send_buffer(&self, endpoint: &dyn UdpEndpoint, buf: &[u8], dest: SocketAddr) {
        use std::sync::atomic::Ordering::Relaxed;
        self.metrics
            .outbound_message_bytes_total
            .fetch_add(buf.len() as u64, Relaxed);
        self.metrics.outbound_messages_total.fetch_add(1, Relaxed);
        // Errors are logged-and-swallowed in the source; a send failure must
        // not abort the owner. (No tracing dep here yet — drop silently.)
        let _ = endpoint.send_to(buf, dest).await;
    }

    /// Offer an ordinary event to the bounded output queue. Producers NEVER block
    /// — a full queue drops the newest and counts it (drop-newest), so backpressure
    /// never reaches the recv path. Correct for events the protocol will resend if
    /// lost (inbound non-INVITE requests, provisionals, 2xx that the UAS keeps
    /// retransmitting until ACKed). Port of `emit`.
    fn emit(&mut self, event: TransactionEvent) {
        self.offer(event, false);
    }

    /// Offer a CRITICAL one-shot event — its only delivery, because the layer has
    /// already consumed its protocol-level redelivery (deleted the client txn,
    /// auto-ACKed a non-2xx final, answered a CANCEL, or 100-silenced an inbound
    /// INVITE). A full queue DEFERS it onto the retry deque instead of dropping it,
    /// so the consumer always sees it once capacity returns.
    fn emit_critical(&mut self, event: TransactionEvent) {
        self.offer(event, true);
    }

    fn offer(&mut self, event: TransactionEvent, critical: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        // Preserve FIFO: once a critical backlog exists, queue further criticals
        // behind it rather than letting a fresh one jump the deferred ones.
        if critical && !self.deferred_events.is_empty() {
            let reason = EventQueueDropReason::of(&event);
            self.metrics.event_queue_drops[reason.index()].fetch_add(1, Relaxed);
            self.deferred_events.push_back(event);
            self.arm_event_retry();
            return;
        }
        let reason = EventQueueDropReason::of(&event);
        match self.events_tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(ev)) => {
                self.metrics.event_queue_drops[reason.index()].fetch_add(1, Relaxed);
                if critical {
                    self.deferred_events.push_back(ev);
                    self.arm_event_retry();
                }
                // else: ordinary event, drop-newest (counted above).
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Consumer gone — the owner winds down via its other arms; do not
                // spin retrying into a closed channel.
                self.deferred_events.clear();
                self.event_retry_armed = false;
            }
        }
    }

    fn arm_event_retry(&mut self) {
        if !self.event_retry_armed {
            self.timers.insert(Timer::EventRetry, ms(EVENT_RETRY_MS));
            self.event_retry_armed = true;
        }
    }

    /// Re-offer deferred critical events in FIFO order once queue capacity returns.
    /// A still-full queue re-arms the tick; a closed channel clears the backlog.
    fn flush_deferred(&mut self) {
        while let Some(ev) = self.deferred_events.pop_front() {
            match self.events_tx.try_send(ev) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(ev)) => {
                    self.deferred_events.push_front(ev);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.deferred_events.clear();
                    self.event_retry_armed = false;
                    return;
                }
            }
        }
        if self.deferred_events.is_empty() {
            self.event_retry_armed = false;
        } else {
            self.arm_event_retry();
        }
    }

    /// Emit any CallQuiesced notices `delete_txn` deferred this turn — AFTER the
    /// turn's protocol events (ADR-0014 ordering). Re-checks under the lockstep
    /// index: a later effect this turn could have re-armed a txn for the call, in
    /// which case the still-armed watch re-fires on its eventual last delete.
    fn flush_pending_quiesce(&mut self) {
        if self.pending_quiesce.is_empty() {
            return;
        }
        for cr in std::mem::take(&mut self.pending_quiesce) {
            if self.self_release_watch.contains(&cr) && !self.has_txns_for(&cr) {
                self.notify_quiesced(cr);
            }
        }
    }

    // ── timer scheduling ─────────────────────────────────────────────────────

    fn start_client_retransmit(
        &mut self,
        branch: &str,
        buf: Vec<u8>,
        dest: SocketAddr,
        max_ms: u64,
    ) {
        let r_key = self.timers.insert(Timer::ClientRetransmit(branch.to_string()), ms(T1));
        let t_key = self.timers.insert(Timer::ClientTimeout(branch.to_string()), ms(max_ms));
        if let Some(txn) = self.txns.get_mut(branch) {
            txn.retransmit_key = Some(r_key);
            txn.timeout_key = Some(t_key);
            txn.retransmit_buf = Some(buf);
            txn.retransmit_interval_ms = T1;
            txn.retransmit_elapsed_ms = T1;
            txn.retransmit_max_ms = max_ms;
            txn.destination = Some(dest);
        }
    }

    // ── timer firing ─────────────────────────────────────────────────────────

    async fn fire_timer(&mut self, endpoint: &dyn UdpEndpoint, timer: Timer) {
        // A fired entry has already left the `DelayQueue` (`poll_expired` freed its
        // slab slot), so the `Key` we still hold for it is now STALE — the next
        // `insert` reuses that exact slot and yields the SAME `Key`. Null the field
        // BEFORE any further work, so a later `cancel_timer`/`delete_txn` can never
        // `try_remove` a reused slot and evict an unrelated live timer (the
        // CLAUDE.md no-generation aliasing hazard). `fire_retransmit` re-sets the
        // retransmit `Key` if it reschedules; the no-reschedule path leaves None.
        match timer {
            Timer::ClientRetransmit(branch) => {
                if let Some(t) = self.txns.get_mut(&branch) {
                    t.retransmit_key = None;
                }
                self.fire_retransmit(endpoint, &branch).await
            }
            Timer::ClientTimeout(branch) => {
                if let Some(t) = self.txns.get_mut(&branch) {
                    t.timeout_key = None;
                }
                self.fire_timeout(&branch)
            }
            Timer::ServerCleanup(branch) => {
                if let Some(t) = self.txns.get_mut(&branch) {
                    t.cleanup_key = None;
                }
                self.delete_txn(&branch);
            }
            Timer::EventRetry => {
                self.event_retry_armed = false;
                self.flush_deferred();
            }
        }
    }

    /// Deliver the one-shot `CallQuiesced` for a watched, txn-free call. It is the
    /// ONLY self-release trigger the router gets for a takeover copy, so it rides
    /// the lossless critical path (`emit_critical`): a full queue defers it onto
    /// the retry deque instead of dropping it (the old drop-newest `emit` stranded
    /// the copy double-serving until its 1 h `GlobalDuration` backstop, exactly in
    /// the post-failover storm where takeover copies exist). The watch is cleared
    /// here because the event is now captured — the deque guarantees its delivery.
    fn notify_quiesced(&mut self, call_ref: String) {
        self.self_release_watch.remove(&call_ref);
        self.emit_critical(TransactionEvent::CallQuiesced { call_ref });
    }

    async fn fire_retransmit(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let (buf, dest, kind, interval, elapsed, max) = match self.txns.get(branch) {
            Some(t) if t.state.is_active() => match (&t.retransmit_buf, t.destination) {
                (Some(buf), Some(dest)) => (
                    buf.clone(),
                    dest,
                    t.kind,
                    t.retransmit_interval_ms,
                    t.retransmit_elapsed_ms,
                    t.retransmit_max_ms,
                ),
                _ => return,
            },
            _ => return, // completed/terminated/gone — stop retransmitting
        };

        self.send_buffer(endpoint, &buf, dest).await;

        // Next interval: INVITE doubles unbounded; non-INVITE caps at T2.
        let next_interval = match kind {
            TxnKind::Invite => interval * 2,
            TxnKind::NonInvite => std::cmp::min(interval * 2, T2),
        };
        let next_elapsed = elapsed + next_interval;
        if next_elapsed < max {
            let key = self
                .timers
                .insert(Timer::ClientRetransmit(branch.to_string()), ms(next_interval));
            if let Some(txn) = self.txns.get_mut(branch) {
                txn.retransmit_key = Some(key);
                txn.retransmit_interval_ms = next_interval;
                txn.retransmit_elapsed_ms = next_elapsed;
            }
        }
    }

    fn fire_timeout(&mut self, branch: &str) {
        let (call_ref, leg_id, method) = match self.txns.get(branch) {
            Some(t) if t.state.is_active() => {
                let method = t
                    .original_request
                    .as_ref()
                    .map(|r| r.method.to_string())
                    .or_else(|| match t.kind {
                        TxnKind::Invite => Some("INVITE".to_string()),
                        TxnKind::NonInvite => None,
                    });
                (t.call_ref.clone(), t.leg_id.clone(), method)
            }
            _ => return,
        };
        self.delete_txn(branch);
        // Critical: the txn is gone and Timer B/F cancelled, so nothing re-fires —
        // a dropped Timeout would strand the leg until the 1 h GlobalDuration.
        self.emit_critical(TransactionEvent::Timeout {
            branch: branch.to_string(),
            call_ref,
            leg_id,
            method,
        });
    }

    fn sweep(&mut self) {
        let stale: Vec<String> = self
            .txns
            .iter()
            .filter(|(_, t)| t.created_at.elapsed() > sweep_max_age(t))
            .map(|(b, _)| b.clone())
            .collect();
        for branch in stale {
            self.delete_txn(&branch);
        }
    }

    // ── send API command handling ─────────────────────────────────────────────

    async fn handle_command(&mut self, endpoint: &dyn UdpEndpoint, cmd: Command) {
        match cmd {
            Command::SendRequest {
                msg,
                dest,
                txn_type,
                reply,
            } => {
                let handle = self.do_send_request(endpoint, *msg, dest, txn_type).await;
                let _ = reply.send(handle);
            }
            Command::SendResponse { msg, dest, reply } => {
                self.do_send_response(endpoint, *msg, dest).await;
                let _ = reply.send(());
            }
            Command::SendRaw { buf, dest, reply } => {
                // sendRaw bypasses transaction management AND the byte counters
                // (matches the source `sendRaw`).
                let _ = endpoint.send_to(&buf, dest).await;
                let _ = reply.send(());
            }
            Command::CancelTxnsForCall { call_ref, reply } => {
                self.do_cancel_txns_for_call(&call_ref);
                let _ = reply.send(());
            }
            Command::ActiveTxnCount { call_ref, reply } => {
                let n = self.txn_index.get(call_ref.as_str()).map_or(0, |s| s.len());
                let _ = reply.send(n);
            }
            Command::WatchSelfRelease { call_ref, reply } => {
                if self.has_txns_for(&call_ref) {
                    // Live txns — arm the watch; the last `delete_txn` fires the
                    // (lossless) CallQuiesced at end-of-turn.
                    self.self_release_watch.insert(call_ref);
                } else {
                    // Already quiesced — capture CallQuiesced now (lossless path).
                    self.notify_quiesced(call_ref);
                }
                let _ = reply.send(());
            }
        }
    }

    /// Any transaction (any role/state) still attributed to `call_ref`? O(1) via
    /// the lockstep `txn_index` (an empty branch set has no entry).
    fn has_txns_for(&self, call_ref: &str) -> bool {
        self.txn_index.contains_key(call_ref)
    }

    async fn do_send_request(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipRequest,
        dest: SocketAddr,
        txn_type: TxnKind,
    ) -> ClientTransactionHandle {
        // Wrap by value to serialize (avoids a full request clone just to make a
        // `&SipMessage`), then destructure `msg` back out for the rest.
        let wrapped = SipMessage::Request(msg);
        let buf = serialize(&wrapped);
        let SipMessage::Request(msg) = wrapped else { unreachable!("just wrapped a request") };
        let branch = msg
            .via
            .first()
            .branch
            .clone()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| self.id_gen.new_branch());
        let (call_ref, leg_id) = extract_via_custom_params(&msg);

        let txn = Transaction {
            branch: branch.clone(),
            role: TxnRole::Client,
            kind: txn_type,
            method: msg.method.to_string(),
            call_id: msg.call_id.clone(),
            from_tag: msg.from.tag.clone().unwrap_or_default(),
            original_request: matches!(txn_type, TxnKind::Invite).then(|| msg.clone()),
            last_response: None,
            last_response_status: None,
            call_ref,
            leg_id,
            state: TxnState::Trying,
            destination: Some(dest),
            created_at: tokio::time::Instant::now(),
            uas_to_tag: None,
            retransmit_key: None,
            timeout_key: None,
            cleanup_key: None,
            retransmit_buf: None,
            retransmit_interval_ms: T1,
            retransmit_elapsed_ms: T1,
            retransmit_max_ms: TIMER_B,
        };
        self.set_txn(txn);

        self.send_buffer(endpoint, &buf, dest).await;
        self.start_client_retransmit(&branch, buf, dest, client_timeout_ms(txn_type, &msg));

        match txn_type {
            TxnKind::Invite => ClientTransactionHandle::Invite {
                branch,
                original_invite: msg,
                destination: dest,
            },
            TxnKind::NonInvite => ClientTransactionHandle::NonInvite {
                branch,
                original_request: msg,
                destination: dest,
            },
        }
    }

    async fn do_send_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipResponse,
        dest: SocketAddr,
    ) {
        let status = msg.status;
        let branch = msg.via.first().branch.clone();
        let buf = serialize(&SipMessage::Response(msg.clone()));

        if let Some(branch) = branch {
            if let Some(txn) = self.txns.get_mut(&branch) {
                if txn.role == TxnRole::Server {
                    // RFC 3261 §17.2.1: a Completed server txn has already sent its
                    // final and now only retransmits the STORED response on a
                    // request retransmit. DROP any further final from the TU here
                    // (a 200 racing handle_cancel's autonomous 487, or a duplicate
                    // relayed final) — re-sending it would put a second final with a
                    // different To-tag on the wire, flip the ACK 2xx/non-2xx
                    // classifier (last_response_status), and orphan a duplicate
                    // Timer H/J. The stored final + dup-absorption cover retransmits.
                    if txn.state == TxnState::Completed {
                        return;
                    }

                    let is_final = status >= 200;
                    let outbound_to_tag = if status > 100 { msg.to.tag.clone() } else { None };
                    // Pin the UAS To-tag on the first >100 response (§17.2.1).
                    if txn.uas_to_tag.is_none() {
                        txn.uas_to_tag = outbound_to_tag;
                    }
                    txn.last_response = Some(buf.clone());
                    txn.last_response_status = Some(status);
                    txn.state = if is_final {
                        TxnState::Completed
                    } else {
                        TxnState::Proceeding
                    };
                    // Free memory on completion — only lastResponse is needed
                    // for retransmit absorption.
                    if is_final {
                        txn.original_request = None;
                        // Schedule Timer H/J cleanup (disjoint-field borrow: txns
                        // and timers are separate Owner fields).
                        let delay = match txn.kind {
                            TxnKind::Invite => TIMER_H,
                            TxnKind::NonInvite => TIMER_J,
                        };
                        let key = self.timers.insert(Timer::ServerCleanup(branch.clone()), ms(delay));
                        if let Some(txn) = self.txns.get_mut(&branch) {
                            txn.cleanup_key = Some(key);
                        }
                    }
                }
            }
        }

        self.send_buffer(endpoint, &buf, dest).await;
    }

    fn do_cancel_txns_for_call(&mut self, call_ref: &str) {
        // O(k-branches) over just this call's live branches (the lockstep index),
        // not an O(total_txns) scan of the whole map.
        //
        // CLIENT transactions only (matching the doc + the TS source, whose server
        // txns carried no callRef). The eviction's job is to stop Timer B/F firing
        // against a vanished call — both are client timers. A SERVER txn must be
        // left to its own Timer H/J: cancelling a Completed BYE/INVITE server txn
        // here would drop its retransmit-absorption window (RFC 3261 §17.2.1/§17.2.2),
        // so a retransmitted request after teardown builds a fresh txn and 481s
        // upstream instead of replaying the cached final.
        let branches: Vec<String> = match self.txn_index.get(call_ref) {
            Some(set) => set.iter().cloned().collect(),
            None => return,
        };
        for branch in branches {
            let is_client = self
                .txns
                .get(&branch)
                .is_some_and(|t| t.role == TxnRole::Client);
            if is_client && self.delete_txn(&branch) {
                self.metrics
                    .txn_cancelled_on_call_evict
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    // ── inbound packet processing ─────────────────────────────────────────────

    async fn handle_packet(&mut self, endpoint: &dyn UdpEndpoint, packet: sip_net::UdpPacket) {
        use std::sync::atomic::Ordering::Relaxed;
        let parsed = match self.parser.parse(&packet.raw) {
            Ok(m) => m,
            Err(_e) => return, // parse error: the source logs a WARN; drop here
        };
        self.metrics.messages_processed.fetch_add(1, Relaxed);
        self.metrics
            .inbound_message_bytes_total
            .fetch_add(packet.raw.len() as u64, Relaxed);

        match parsed {
            SipMessage::Request(req) => self.handle_inbound_request(endpoint, req, packet.src).await,
            SipMessage::Response(resp) => {
                self.handle_inbound_response(endpoint, resp, packet.src).await
            }
        }
    }

    async fn handle_inbound_request(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: SipRequest,
        src: SocketAddr,
    ) {
        let branch = req.via.first().branch.clone().unwrap_or_default();

        if branch.is_empty() {
            // No branch — pass through (pre-RFC 3261 UA).
            self.emit(TransactionEvent::Message {
                message: Box::new(SipMessage::Request(req)),
                src,
            });
            return;
        }

        // ── ACK ──────────────────────────────────────────────────────────────
        if req.method == "ACK" {
            if let Some(existing) = self.txns.get(&branch) {
                if existing.role == TxnRole::Server
                    && existing.kind == TxnKind::Invite
                    && existing.state == TxnState::Completed
                {
                    match existing.last_response_status {
                        // ACK for non-2xx (3xx-6xx) — absorb, terminate.
                        Some(s) if s >= 300 => {
                            self.delete_txn(&branch);
                            return;
                        }
                        // ACK for 2xx — pass through to app, terminate.
                        Some(s) if (200..300).contains(&s) => {
                            self.delete_txn(&branch);
                            self.emit(TransactionEvent::Message {
                                message: Box::new(SipMessage::Request(req)),
                                src,
                            });
                            return;
                        }
                        _ => {}
                    }
                }
            }
            // ACK with no matching server txn. A stateless-503 ACK carries no
            // To-tag and must be absorbed (not propagated); a legitimate 2xx
            // ACK always has a To-tag and passes through.
            if req.to.tag.is_none() {
                return;
            }
            self.emit(TransactionEvent::Message {
                message: Box::new(SipMessage::Request(req)),
                src,
            });
            return;
        }

        // ── CANCEL ─────────────────────────────────────────────────────────────
        if req.method == "CANCEL" {
            self.handle_cancel(endpoint, req, src).await;
            return;
        }

        // ── Duplicate detection for other requests ─────────────────────────────
        if let Some(existing) = self.txns.get(&branch) {
            if let Some(cached) = existing.last_response.clone() {
                self.send_buffer(endpoint, &cached, src).await;
            }
            // else: duplicate with no response yet — absorb silently.
            return;
        }

        // NOTE: the source's Tier-3 overload admission gate (overload.shouldAdmit
        // + stateless 503) is intentionally NOT ported here — it depends on
        // OverloadController / AppConfig (b2bua slice). See docs/adr/0007
        // "Deferred". This layer admits unconditionally for now.

        // ── New server transaction ─────────────────────────────────────────────
        let kind = if req.method == "INVITE" {
            TxnKind::Invite
        } else {
            TxnKind::NonInvite
        };
        let is_invite = matches!(kind, TxnKind::Invite);

        // Attribute the server txn to its call so the B2BUA's acting-backup
        // self-release (ADR-0014) can count "transactions still serving this
        // call". An in-dialog request the proxy routes to the B2BUA carries the
        // `callRef` in its Request-URI (the dialog remote target = the B2BUA
        // Contact, which stamps it) — the same key the router resolves on. An
        // out-of-dialog request (initial INVITE / OPTIONS keepalive) has no
        // `callRef` param yet → `None`, as before.
        let call_ref = extract_ruri_call_ref(&req);

        let txn = Transaction {
            branch: branch.clone(),
            role: TxnRole::Server,
            kind,
            method: req.method.to_string(),
            call_id: req.call_id.clone(),
            from_tag: req.from.tag.clone().unwrap_or_default(),
            // INVITE server txns keep the request for the CANCEL→487 path; a
            // non-INVITE server txn never reads it, so skip that clone.
            original_request: is_invite.then(|| req.clone()),
            last_response: None,
            last_response_status: None,
            call_ref,
            leg_id: None,
            state: TxnState::Trying,
            destination: None,
            created_at: tokio::time::Instant::now(),
            uas_to_tag: None,
            retransmit_key: None,
            timeout_key: None,
            cleanup_key: None,
            retransmit_buf: None,
            retransmit_interval_ms: T1,
            retransmit_elapsed_ms: T1,
            retransmit_max_ms: TIMER_B,
        };
        self.set_txn(txn);

        // For INVITE, immediately send 100 Trying and move to proceeding.
        if is_invite {
            let trying = generate_response(&req, 100, "Trying", &GenerateResponseOpts::default());
            let trying_buf = serialize(&SipMessage::Response(trying));
            self.send_buffer(endpoint, &trying_buf, src).await;
            if let Some(txn) = self.txns.get_mut(&branch) {
                txn.state = TxnState::Proceeding;
                // Cache the 100 as the latest provisional so a retransmitted INVITE
                // replays it (RFC 3261 §17.2.1) instead of being absorbed silently —
                // the auto-100 already silenced the UAC's own retransmission timer.
                txn.last_response = Some(trying_buf);
                txn.last_response_status = Some(100);
            }
        }

        // Critical for INVITE: the 100 we just sent stops the UAC retransmitting,
        // so this Message is the app's ONLY notice of the call — a drop would leave
        // a timer-less server txn squatting until the sweep while the caller hears
        // 100-then-silence. Non-INVITE requests stay lossy (the UAC resends them).
        let event = TransactionEvent::Message {
            message: Box::new(SipMessage::Request(req)),
            src,
        };
        if is_invite {
            self.emit_critical(event);
        } else {
            self.emit(event);
        }
    }

    async fn handle_cancel(&mut self, endpoint: &dyn UdpEndpoint, req: SipRequest, src: SocketAddr) {
        let call_id = req.call_id.clone();
        let from_tag = req.from.tag.clone().unwrap_or_default();

        // Find the matching ACTIVE INVITE server txn. The CANCEL shares the
        // INVITE's top-Via branch (RFC 3261 §9.1), so a compliant peer keys it
        // directly — an O(1) `get` instead of an O(total_txns) scan; we still
        // confirm callId+fromTag (and fall back to the scan for a peer that didn't
        // preserve the branch).
        let cancel_branch = req.via.first().branch.clone().unwrap_or_default();
        let is_cancel_target = |t: &Transaction| {
            t.role == TxnRole::Server
                && t.kind == TxnKind::Invite
                && t.call_id == call_id
                && t.from_tag == from_tag
                && t.state.is_active()
        };
        let matched_branch = self
            .txns
            .get(&cancel_branch)
            .filter(|t| is_cancel_target(t))
            .map(|_| cancel_branch.clone())
            .or_else(|| {
                self.txns
                    .iter()
                    .find_map(|(b, t)| is_cancel_target(t).then(|| b.clone()))
            });

        // RFC 3261 §9.2: a CANCEL matching no active INVITE server txn gets a 481
        // and has NO effect. Emitting a 200 + a Cancelled event here (the old
        // unconditional path) let a late/retransmitted/replayed CANCEL — one
        // arriving after the call was answered (txn Completed, not active) — tear
        // down an established call upstream, and gave the retransmitted CANCEL a
        // tagless 200 plus a duplicate Cancelled. Reject it cleanly instead.
        let branch = match matched_branch {
            Some(b) => b,
            None => {
                let reject = generate_response(
                    &req,
                    481,
                    "Call/Transaction Does Not Exist",
                    &GenerateResponseOpts::default(),
                );
                self.send_buffer(endpoint, &serialize(&SipMessage::Response(reject)), src)
                    .await;
                return;
            }
        };

        // Resolve (and lazily pin) the UAS To-tag on the matched INVITE.
        let mut uas_to_tag = self.txns.get(&branch).and_then(|t| t.uas_to_tag.clone());
        if uas_to_tag.is_none() {
            let pinned = self.id_gen.new_tag();
            if let Some(txn) = self.txns.get_mut(&branch) {
                txn.uas_to_tag = Some(pinned.clone());
            }
            uas_to_tag = Some(pinned);
        }

        // 200 OK to the CANCEL itself.
        let cancel_ok = generate_response(
            &req,
            200,
            "OK",
            &GenerateResponseOpts {
                to_tag: uas_to_tag.clone(),
                ..Default::default()
            },
        );
        self.send_buffer(endpoint, &serialize(&SipMessage::Response(cancel_ok)), src)
            .await;

        // 487 Request Terminated on the matched INVITE.
        let original = self
            .txns
            .get(&branch)
            .and_then(|t| t.original_request.clone());
        if let Some(original) = original {
            let terminated = generate_response(
                &original,
                487,
                "Request Terminated",
                &GenerateResponseOpts {
                    to_tag: uas_to_tag,
                    ..Default::default()
                },
            );
            let terminated_buf = serialize(&SipMessage::Response(terminated));
            self.send_buffer(endpoint, &terminated_buf, src).await;
            if let Some(txn) = self.txns.get_mut(&branch) {
                txn.state = TxnState::Completed;
                txn.last_response = Some(terminated_buf);
                txn.last_response_status = Some(487);
                txn.original_request = None;
            }
            // Timer-H-487 cleanup if the ACK for 487 never arrives.
            let key = self.timers.insert(Timer::ServerCleanup(branch.clone()), ms(TIMER_H));
            if let Some(txn) = self.txns.get_mut(&branch) {
                txn.cleanup_key = Some(key);
            }
        }

        // Critical: we already answered 200 + 487 on the wire; a dropped Cancelled
        // would leave the b-leg ringing a cancelled call (no other signal upstream).
        self.emit_critical(TransactionEvent::Cancelled { call_id, from_tag });
    }

    async fn handle_inbound_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        resp: SipResponse,
        src: SocketAddr,
    ) {
        let branch = resp.via.first().branch.clone().unwrap_or_default();

        // 100 Trying: absorb after nudging the matching client txn's state.
        if resp.status == 100 {
            if !branch.is_empty() {
                let key = match self.txns.get_mut(&branch) {
                    Some(txn) if txn.role == TxnRole::Client => {
                        txn.state = TxnState::Proceeding;
                        txn.retransmit_key.take()
                    }
                    _ => None,
                };
                self.cancel_timer(key);
            }
            return;
        }

        if !branch.is_empty() {
            // CANCEL responses reuse the INVITE branch — never match them to the
            // INVITE client txn (would tear it down on the 200 and miss the 487).
            if resp.cseq.method.as_str().eq_ignore_ascii_case("CANCEL") {
                self.emit(TransactionEvent::Message {
                    message: Box::new(SipMessage::Response(resp)),
                    src,
                });
                return;
            }

            // Snapshot what we need before mutating.
            let client_match = self
                .txns
                .get(&branch)
                .filter(|t| t.role == TxnRole::Client)
                .map(|t| (t.kind, t.original_request.clone(), t.destination));

            if let Some((kind, original_request, destination)) = client_match {
                if resp.status < 200 {
                    // Provisional 1xx>100 — proceeding, stop retransmit.
                    let key = match self.txns.get_mut(&branch) {
                        Some(txn) => {
                            txn.state = TxnState::Proceeding;
                            txn.retransmit_key.take()
                        }
                        None => None,
                    };
                    self.cancel_timer(key);
                } else {
                    // Final — stop timers, auto-ACK non-2xx for INVITE, delete.
                    if kind == TxnKind::Invite && resp.status >= 300 {
                        if let (Some(orig), Some(dest)) = (original_request, destination) {
                            let ack = generate_ack_for_non_2xx(&orig, &resp);
                            self.send_buffer(endpoint, &serialize(&SipMessage::Request(ack)), dest)
                                .await;
                        }
                    }
                    self.delete_txn(&branch);
                    // Critical: we deleted our client txn (consuming our request
                    // retransmission) and auto-ACKed any non-2xx (silencing the
                    // UAS's resend of that final), so this is the app's only
                    // delivery of the final response.
                    self.emit_critical(TransactionEvent::Message {
                        message: Box::new(SipMessage::Response(resp)),
                        src,
                    });
                    return;
                }
            }
            // (a response landing on a server txn is anomalous — pass through)
        }

        // Provisionals and unmatched responses are protocol-redelivered → lossy.
        self.emit(TransactionEvent::Message {
            message: Box::new(SipMessage::Response(resp)),
            src,
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Per-txn safety-net age for the sweep. A still-ringing INVITE (no final
/// response yet — an inbound INVITE awaiting the app's answer, or an outbound
/// INVITE past its retransmit window) legitimately outlives the 35 s net: a
/// callee may ring for minutes and the no-answer timer / long initial-INVITE
/// Timer B owns that deadline. Give it a backstop just above that long timeout so
/// the net never reaps a live call. Everything else — completed txns governed by
/// Timer H/J, all non-INVITE — keeps the tight 35 s net just above 32 s, so the
/// sweep still only ever catches what a missing-cleanup bug would otherwise leak.
fn sweep_max_age(t: &Transaction) -> std::time::Duration {
    match (t.kind, t.state) {
        (TxnKind::Invite, TxnState::Trying | TxnState::Proceeding) => {
            ms(INVITE_INITIAL_TIMEOUT + TXN_MAX_AGE)
        }
        _ => ms(TXN_MAX_AGE),
    }
}

/// RFC 3261 §17.1 client-transaction timeout (Timer B/F). Differentiated for
/// INVITE: an INITIAL (out-of-dialog — no To-tag) INVITE is a call setup whose
/// ring time the upper layer's no-answer timer owns, so it gets the long
/// [`INVITE_INITIAL_TIMEOUT`] backstop (below the 180 s Timer-C mark, above any
/// deployment no-answer timeout). An in-dialog re-INVITE (To-tag present) and
/// every non-INVITE keep the 32 s failure-detection timeout.
fn client_timeout_ms(kind: TxnKind, req: &SipRequest) -> u64 {
    match kind {
        TxnKind::Invite if req.to.tag.is_none() => INVITE_INITIAL_TIMEOUT,
        TxnKind::Invite => TIMER_B,
        TxnKind::NonInvite => TIMER_F,
    }
}

/// Extract + URL-decode the Via `cr` (callRef) / `lg` (legId) custom params.
/// Production `buildCallVia` URL-encodes both (callRefs contain `|`/`@`); the
/// parser stores raw param strings, so we decode here so `cancel_txns_for_call`
/// matches the natural callRef the caller passes (see the cr/lg round-trip
/// regression test).
fn extract_via_custom_params(req: &SipRequest) -> (Option<String>, Option<String>) {
    let params = &req.via.first().params;
    let read = |name: &str| match params.get(name) {
        Some(ParamValue::Value(v)) => Some(decode_param(v)),
        _ => None,
    };
    (read("cr"), read("lg"))
}

/// Extract + URL-decode the Request-URI `callRef` param (percent-encoded by the
/// B2BUA's `build_call_contact`). `parse_uri_params` lower-cases param names per
/// RFC 3261 §19.1.1, so the key is `callref`. `None` for an out-of-dialog request
/// (no param). Used to attribute a server transaction to its call (ADR-0014
/// self-release counting).
fn extract_ruri_call_ref(req: &SipRequest) -> Option<String> {
    sip_message::message_helpers::parse_uri_params(&req.uri)
        .get("callref")
        .map(|v| decode_param(v))
}

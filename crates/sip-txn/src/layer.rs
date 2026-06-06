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

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::generators::{generate_ack_for_non_2xx, generate_response, GenerateResponseOpts};
use sip_message::{serialize, ParamValue, SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::sync::{mpsc, oneshot};
use tokio_util::time::{delay_queue::Key, DelayQueue};

use crate::event::{ClientTransactionHandle, EventQueueDropReason, TransactionEvent, TxnKind};
use crate::metrics::{MetricsInner, TransactionMetrics};
use crate::rng::IdGen;
use crate::timers::{ms, T1, T2, TIMER_B, TIMER_F, TIMER_H, TIMER_J, TXN_MAX_AGE, TXN_SWEEP_INTERVAL};

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
}

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
            txn_counts: HashMap::new(),
            timers: DelayQueue::new(),
            parser,
            events_tx,
            metrics: metrics_inner,
            id_gen: config.id_gen,
            self_release_watch: HashSet::new(),
        };

        tokio::spawn(run(owner, endpoint, cmd_rx));

        (Self { cmd_tx, metrics }, events_rx)
    }

    pub fn metrics(&self) -> &TransactionMetrics {
        &self.metrics
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
    /// last-`delete_txn` for a watched call we emit
    /// [`TransactionEvent::CallQuiesced`] and drop the watch. Bounded — one entry
    /// per live takeover copy, removed when it fires.
    self_release_watch: HashSet<String>,
    /// `call_ref → live-txn count`, kept in **lockstep** with `txns` (every
    /// `set_txn`/`delete_txn` updates both; an entry is dropped at count 0). Makes
    /// the acting-backup self-release machinery — `has_txns_for`,
    /// `active_txn_count_for_call`, and the last-`delete_txn` check — O(1) instead
    /// of an O(total_txns) scan of the branch-keyed map, which on a backup serving
    /// many failed-over dialogs ran per teardown under endurance load. Same
    /// single-writer discipline CLAUDE.md prescribes for the timer driver's
    /// `active`/queue lockstep: never let this drift from `txns`.
    txn_counts: HashMap<String, usize>,
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
    // The first `interval` tick is immediate; skip it (the source's sweep
    // fiber sleeps before its first pass).
    sweep.tick().await;

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => match cmd {
                Some(c) => owner.handle_command(endpoint, c).await,
                None => break, // all handles dropped → shut down
            },
            packet = endpoint.recv() => match packet {
                Some(p) => owner.handle_packet(endpoint, p).await,
                None => break, // endpoint closed
            },
            timer = next_expired(&mut owner.timers), if !owner.timers.is_empty() => {
                owner.fire_timer(endpoint, timer).await;
            }
            _ = sweep.tick() => owner.sweep(),
        }
    }
}

impl Owner {
    // ── map bookkeeping (keeps the active-txn gauge == map size) ────────────

    fn set_txn(&mut self, txn: Transaction) {
        let new_call_ref = txn.call_ref.clone();
        // A re-insert of the same branch (rare) must not double-count: drop the
        // displaced txn's contribution before adding the new one.
        if let Some(old) = self.txns.insert(txn.branch.clone(), txn) {
            self.untrack_call_ref(&old.call_ref);
        }
        self.track_call_ref(&new_call_ref);
        self.sync_active();
    }

    /// `call_ref → txn-count` increment, in lockstep with a `txns` insert. A txn
    /// with no `call_ref` (out-of-dialog initial INVITE / OPTIONS) is not counted.
    fn track_call_ref(&mut self, call_ref: &Option<String>) {
        if let Some(cr) = call_ref {
            *self.txn_counts.entry(cr.clone()).or_insert(0) += 1;
        }
    }

    /// `call_ref → txn-count` decrement, in lockstep with a `txns` remove; the
    /// entry is dropped at 0 so `has_txns_for` is a plain `contains_key`.
    fn untrack_call_ref(&mut self, call_ref: &Option<String>) {
        if let Some(cr) = call_ref {
            if let Some(n) = self.txn_counts.get_mut(cr) {
                *n -= 1;
                if *n == 0 {
                    self.txn_counts.remove(cr);
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
                self.untrack_call_ref(&t.call_ref);
                self.sync_active();
                // ADR-0014 self-release: if this was the LAST transaction for a
                // watched call, notify the consumer so it can shed its acting-backup
                // takeover copy. (For a 2xx INVITE the server txn lingers in
                // `Completed` until Timer H — the ACK reuses a different branch — so
                // this naturally fires at Timer H, after the ACK was relayed.)
                if let Some(cr) = t.call_ref {
                    if self.self_release_watch.contains(&cr) && !self.has_txns_for(&cr) {
                        self.self_release_watch.remove(&cr);
                        self.emit(TransactionEvent::CallQuiesced { call_ref: cr });
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

    /// Offer an event to the bounded output queue. Producers NEVER block — a
    /// full queue drops the newest and counts it (drop-newest), so backpressure
    /// never reaches the recv path. Port of `emit`.
    fn emit(&self, event: TransactionEvent) {
        let reason = EventQueueDropReason::of(&event);
        if self.events_tx.try_send(event).is_err() {
            self.metrics.event_queue_drops[reason.index()]
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    // ── timer scheduling ─────────────────────────────────────────────────────

    fn start_client_retransmit(
        &mut self,
        branch: &str,
        buf: Vec<u8>,
        dest: SocketAddr,
        kind: TxnKind,
    ) {
        let max = match kind {
            TxnKind::Invite => TIMER_B,
            TxnKind::NonInvite => TIMER_F,
        };
        let r_key = self.timers.insert(Timer::ClientRetransmit(branch.to_string()), ms(T1));
        let t_key = self.timers.insert(Timer::ClientTimeout(branch.to_string()), ms(max));
        if let Some(txn) = self.txns.get_mut(branch) {
            txn.retransmit_key = Some(r_key);
            txn.timeout_key = Some(t_key);
            txn.retransmit_buf = Some(buf);
            txn.retransmit_interval_ms = T1;
            txn.retransmit_elapsed_ms = T1;
            txn.retransmit_max_ms = max;
            txn.destination = Some(dest);
        }
    }

    // ── timer firing ─────────────────────────────────────────────────────────

    async fn fire_timer(&mut self, endpoint: &dyn UdpEndpoint, timer: Timer) {
        match timer {
            Timer::ClientRetransmit(branch) => self.fire_retransmit(endpoint, &branch).await,
            Timer::ClientTimeout(branch) => self.fire_timeout(&branch),
            Timer::ServerCleanup(branch) => {
                self.delete_txn(&branch);
            }
        }
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
                    .map(|r| r.method.clone())
                    .or_else(|| match t.kind {
                        TxnKind::Invite => Some("INVITE".to_string()),
                        TxnKind::NonInvite => None,
                    });
                (t.call_ref.clone(), t.leg_id.clone(), method)
            }
            _ => return,
        };
        self.delete_txn(branch);
        self.emit(TransactionEvent::Timeout {
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
            .filter(|(_, t)| t.created_at.elapsed() > ms(TXN_MAX_AGE))
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
                let n = self.txn_counts.get(call_ref.as_str()).copied().unwrap_or(0);
                let _ = reply.send(n);
            }
            Command::WatchSelfRelease { call_ref, reply } => {
                // If the call already has no transactions, fire at once; else arm
                // the watch so the last `delete_txn` emits CallQuiesced.
                if self.has_txns_for(&call_ref) {
                    self.self_release_watch.insert(call_ref);
                } else {
                    self.emit(TransactionEvent::CallQuiesced { call_ref });
                }
                let _ = reply.send(());
            }
        }
    }

    /// Any transaction (any role/state) still attributed to `call_ref`? O(1) via
    /// the lockstep `txn_counts` index (a 0-count call_ref has no entry).
    fn has_txns_for(&self, call_ref: &str) -> bool {
        self.txn_counts.contains_key(call_ref)
    }

    async fn do_send_request(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipRequest,
        dest: SocketAddr,
        txn_type: TxnKind,
    ) -> ClientTransactionHandle {
        let buf = serialize(&SipMessage::Request(msg.clone()));
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
            method: msg.method.clone(),
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
        self.start_client_retransmit(&branch, buf, dest, txn_type);

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
            let is_final = status >= 200;
            let outbound_to_tag = if status > 100 { msg.to.tag.clone() } else { None };
            let mut completed_kind: Option<TxnKind> = None;

            if let Some(txn) = self.txns.get_mut(&branch) {
                if txn.role == TxnRole::Server {
                    if is_final {
                        completed_kind = Some(txn.kind);
                    }
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
                    }
                }
            }

            // Schedule Timer H/J cleanup for completed server transactions.
            if let Some(kind) = completed_kind {
                let delay = match kind {
                    TxnKind::Invite => TIMER_H,
                    TxnKind::NonInvite => TIMER_J,
                };
                let key = self.timers.insert(Timer::ServerCleanup(branch.clone()), ms(delay));
                if let Some(txn) = self.txns.get_mut(&branch) {
                    txn.cleanup_key = Some(key);
                }
            }
        }

        self.send_buffer(endpoint, &buf, dest).await;
    }

    fn do_cancel_txns_for_call(&mut self, call_ref: &str) {
        let victims: Vec<String> = self
            .txns
            .iter()
            .filter(|(_, t)| t.call_ref.as_deref() == Some(call_ref))
            .map(|(b, _)| b.clone())
            .collect();
        for branch in victims {
            if self.delete_txn(&branch) {
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
            method: req.method.clone(),
            call_id: req.call_id.clone(),
            from_tag: req.from.tag.clone().unwrap_or_default(),
            original_request: Some(req.clone()),
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
            }
        }

        self.emit(TransactionEvent::Message {
            message: Box::new(SipMessage::Request(req)),
            src,
        });
    }

    async fn handle_cancel(&mut self, endpoint: &dyn UdpEndpoint, req: SipRequest, src: SocketAddr) {
        let call_id = req.call_id.clone();
        let from_tag = req.from.tag.clone().unwrap_or_default();

        // Find the matching INVITE server txn (CANCEL shares the INVITE's
        // branch, but we match by callId+fromTag per RFC 3261 §9.1) so we can
        // echo its UAS To-tag on the 200/487.
        let matched_branch = self.txns.iter().find_map(|(b, t)| {
            (t.role == TxnRole::Server
                && t.kind == TxnKind::Invite
                && t.call_id == call_id
                && t.from_tag == from_tag
                && t.state.is_active())
            .then(|| b.clone())
        });

        // Resolve (and lazily pin) the UAS To-tag.
        let mut uas_to_tag = matched_branch
            .as_ref()
            .and_then(|b| self.txns.get(b))
            .and_then(|t| t.uas_to_tag.clone());
        if let Some(branch) = &matched_branch {
            if uas_to_tag.is_none() {
                let pinned = self.id_gen.new_tag();
                if let Some(txn) = self.txns.get_mut(branch) {
                    txn.uas_to_tag = Some(pinned.clone());
                }
                uas_to_tag = Some(pinned);
            }
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
        if let Some(branch) = matched_branch {
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
        }

        self.emit(TransactionEvent::Cancelled { call_id, from_tag });
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

        let resp_cseq_method = resp.cseq.method.to_ascii_uppercase();

        if !branch.is_empty() {
            // CANCEL responses reuse the INVITE branch — never match them to the
            // INVITE client txn (would tear it down on the 200 and miss the 487).
            if resp_cseq_method == "CANCEL" {
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
                }
            }
            // (a response landing on a server txn is anomalous — pass through)
        }

        self.emit(TransactionEvent::Message {
            message: Box::new(SipMessage::Response(resp)),
            src,
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract + URL-decode the Via `cr` (callRef) / `lg` (legId) custom params.
/// Production `buildCallVia` URL-encodes both (callRefs contain `|`/`@`); the
/// parser stores raw param strings, so we decode here so `cancel_txns_for_call`
/// matches the natural callRef the caller passes (see the cr/lg round-trip
/// regression test).
fn extract_via_custom_params(req: &SipRequest) -> (Option<String>, Option<String>) {
    let params = &req.via.first().params;
    let read = |name: &str| match params.get(name) {
        Some(ParamValue::Value(v)) => Some(percent_decode(v)),
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
        .map(|v| percent_decode(v))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

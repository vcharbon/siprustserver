//! The owner task: the `select!` loop, transaction-map + timer-wheel
//! bookkeeping (kept in lockstep), timer dispatch, inbound-packet parsing, and
//! the periodic sweep tick. Protocol behavior does NOT live here — the UAC FSM is
//! `layer::client`, the UAS FSM is `layer::server`, output-queue discipline is
//! `layer::events`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::{SipMessage, SipParser};
use sip_net::UdpEndpoint;
use tokio::sync::mpsc;
use tokio_util::time::{delay_queue::Key, DelayQueue};

use crate::event::TransactionEvent;
use crate::metrics::MetricsInner;
use crate::rng::IdGen;
use crate::timers::{ms, TXN_SWEEP_INTERVAL};

use super::backlog::DeferredBound;
use super::handle::{Command, TransactionConfig};
use super::key::{ServerTxnId, ServerTxnIdentity, ServerTxnKey};
use super::refusals::InviteRefusals;
use super::txn::{CancelWire, Timer, Transaction, TxnId, TxnRef, TxnRole};

pub(super) struct Owner {
    /// The client transactions, by the branch of the request this node sent
    /// (RFC 3261 §17.1.3: only a response matches one). Boxed so a bucket is
    /// a pointer, not the multi-KB record: the table's doubling step and its
    /// footprint after a drain scale with the live count only through 32-byte
    /// buckets, and a txn's own memory goes back to the allocator when it is
    /// deleted.
    pub(super) clients: HashMap<String, Box<Transaction>>,
    /// The server transactions, by the §17.2.3 identity of the request that
    /// opened them: top-Via branch, sent-by and method ([`ServerTxnKey`],
    /// looked up by a [`ServerTxnId`] borrowed from the message). Only a
    /// request matches one. Two clients colliding on a branch hold two, and a
    /// client and a server transaction may share a branch, as when a request
    /// comes back to the node that sent it.
    pub(super) servers: HashMap<ServerTxnKey, Box<Transaction>>,
    pub(super) timers: DelayQueue<Timer>,
    pub(super) parser: Arc<dyn SipParser + Send + Sync>,
    pub(super) events_tx: mpsc::Sender<TransactionEvent>,
    pub(super) metrics: Arc<MetricsInner>,
    pub(super) id_gen: Arc<IdGen>,
    /// call_refs the consumer asked to be told about when their **last**
    /// transaction clears (ADR-0014 acting-backup self-release). On the
    /// last-`delete_txn` for a watched call we capture
    /// [`TransactionEvent::CallQuiesced`] onto the lossless critical path and drop
    /// the watch. Bounded — one entry per live takeover copy, removed when it fires.
    pub(super) self_release_watch: HashSet<String>,
    /// `call_ref → its live transactions, by role`, kept in **lockstep** with
    /// `clients` and `servers` (every `set_txn`/`delete_txn` updates both; the
    /// entry is dropped when it empties). Makes the acting-backup self-release
    /// machinery — `has_txns_for`, `active_txn_count_for_call`, the
    /// last-`delete_txn` check — AND the per-call-eviction
    /// `do_cancel_txns_for_call` O(k) in the call's transactions instead of a
    /// scan of the maps. Never let this drift from the maps.
    pub(super) txn_index: HashMap<String, CallTxns>,
    /// CRITICAL events a full events queue deferred, in FIFO order — re-offered on
    /// the [`Timer::EventRetry`] tick, never dropped while their transaction
    /// lives. These are one-shot signals whose protocol-level redelivery the
    /// layer already consumed (a deleted txn's Timeout, an answered CANCEL's
    /// Cancelled, a takeover copy's only CallQuiesced, an auto-ACKed non-2xx
    /// final, an inbound INVITE whose 100 silenced the UAC). New initial INVITEs
    /// are its only entries the consumer does not pace; `deferred_bound` caps
    /// them, and a server transaction leaving `Active` unanswered, at its
    /// backstop, takes its deferred requests with it.
    pub(super) deferred_events: VecDeque<TransactionEvent>,
    /// [`TransactionConfig::deferred_bound`](crate::TransactionConfig).
    pub(super) deferred_bound: Option<DeferredBound>,
    /// [`TransactionConfig::invite_refusals`](crate::TransactionConfig).
    pub(super) refusals: Option<InviteRefusals>,
    /// Whether a [`Timer::EventRetry`] is already in the wheel (at most one).
    pub(super) event_retry_armed: bool,
    /// call_refs whose last txn cleared THIS turn but whose `CallQuiesced` must be
    /// emitted only AFTER the turn's protocol events (the ACK/Timeout that drove the
    /// delete) — else the router self-releases the takeover copy ahead of the very
    /// event it was about, orphaning it. Drained by `flush_pending_quiesce` at the
    /// end of every owner turn.
    pub(super) pending_quiesce: Vec<String>,
    /// The configured INVITE bound
    /// ([`TransactionConfig::invite_initial_timeout_ms`](crate::TransactionConfig)):
    /// the client timeout of an INVITE that has drawn a provisional and the
    /// backstop of every active INVITE transaction both derive from it.
    pub(super) invite_initial_timeout_ms: u64,
    /// The initial INVITE's first-response bound
    /// ([`TransactionConfig::invite_first_response_timeout_ms`](crate::TransactionConfig)):
    /// the client timeout of an out-of-dialog INVITE that has drawn nothing.
    pub(super) invite_first_response_timeout_ms: u64,
    /// The held-CANCEL policy
    /// ([`TransactionConfig::cancel_hold_grace_ms`](crate::TransactionConfig)):
    /// `Some(ms)` bounds the wait for the branch's first provisional — then
    /// the CANCEL is sent regardless (ADR-0028); `None` is the strict §9.1
    /// wait (hold until provisional, drop with the txn).
    pub(super) cancel_hold_grace_ms: Option<u64>,
    /// The To-tag this node bound to a dialog it answered as UAS, by
    /// (Call-ID, From-tag), kept for Timer L past the transaction so a
    /// CANCEL or a stray answered after the transaction is gone still carries
    /// it (RFC 3261 §9.2, §12.1.1). Purged on the sweep tick
    /// (`forget_expired_uas_tags`).
    pub(super) recent_uas_tags: HashMap<(String, String), (String, tokio::time::Instant)>,
    /// [`TransactionConfig::strict_to_tag`](crate::TransactionConfig).
    pub(super) strict_to_tag: bool,
}

/// The live transactions of one call, by role ([`Owner::txn_index`]).
#[derive(Default)]
pub(super) struct CallTxns {
    pub(super) client: HashSet<String>,
    pub(super) server: HashSet<ServerTxnKey>,
}

impl CallTxns {
    pub(super) fn len(&self) -> usize {
        self.client.len() + self.server.len()
    }

    fn is_empty(&self) -> bool {
        self.client.is_empty() && self.server.is_empty()
    }
}

/// Below this many buckets a table is never shrunk: the saving is noise and a
/// small idle node would rehash on every sweep.
const SHRINK_MIN_CAPACITY: usize = 1024;

/// Give a drained table back to the allocator: once it is under a quarter full
/// it is rehashed to twice its length. The factor-of-four gap is the hysteresis
/// that keeps a table oscillating around one size from rehashing every sweep.
pub(super) fn shrink_idle<K: Eq + std::hash::Hash, V>(map: &mut HashMap<K, V>) {
    let cap = map.capacity();
    if cap > SHRINK_MIN_CAPACITY && map.len() < cap / 4 {
        map.shrink_to(map.len() * 2);
    }
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

pub(super) async fn run(
    mut owner: Owner,
    endpoint: Box<dyn UdpEndpoint>,
    mut cmd_rx: mpsc::Receiver<Command>,
) {
    let endpoint: &dyn UdpEndpoint = endpoint.as_ref();
    let mut sweep = tokio::time::interval(ms(TXN_SWEEP_INTERVAL));
    // Coalesce missed ticks: after any owner stall the catch-up must be a single
    // pass, not a burst of back-to-back full-map scans (default `Burst`).
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first `interval` tick is immediate; skip it so the first sweep runs a
    // full interval after start.
    sweep.tick().await;

    loop {
        // `biased` polls the arms top-to-bottom and the INBOUND PACKET arm is LAST
        // on purpose: a sustained packet flood keeps `endpoint.recv()` ready every
        // iteration, so if it were polled first it would starve the timer wheel and
        // the safety-net sweep indefinitely — retransmits/timeouts/cleanups would
        // stall and the transaction maps + DelayQueue would grow unbounded exactly under
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
        // Sample the DelayQueue depth (O(1)) so a timer/slab leak is visible even
        // when active_transactions is flat (the no-chaos RSS-climb localisation).
        owner
            .metrics
            .timer_queue_len
            .store(owner.timers.len(), std::sync::atomic::Ordering::Relaxed);
        owner
            .metrics
            .event_queue_deferred
            .store(owner.deferred_events.len(), std::sync::atomic::Ordering::Relaxed);
    }
}

impl Owner {
    /// The owner over `events_tx`, holding the tunables of `config` (its
    /// `udp_queue_max` already sized `events_tx`).
    pub(super) fn new(
        parser: Arc<dyn SipParser + Send + Sync>,
        events_tx: mpsc::Sender<TransactionEvent>,
        metrics: Arc<MetricsInner>,
        config: TransactionConfig,
    ) -> Self {
        assert!(
            config.deferred_bound.is_none() || config.invite_refusals.is_some(),
            "a deferred bound refuses through invite_refusals"
        );
        Self {
            clients: HashMap::new(),
            servers: HashMap::new(),
            timers: DelayQueue::new(),
            parser,
            events_tx,
            metrics,
            id_gen: config.id_gen,
            self_release_watch: HashSet::new(),
            txn_index: HashMap::new(),
            deferred_events: VecDeque::new(),
            deferred_bound: config.deferred_bound,
            refusals: config.invite_refusals,
            event_retry_armed: false,
            pending_quiesce: Vec::new(),
            invite_initial_timeout_ms: config.invite_initial_timeout_ms,
            invite_first_response_timeout_ms: config.invite_first_response_timeout_ms,
            cancel_hold_grace_ms: config.cancel_hold_grace_ms,
            recent_uas_tags: HashMap::new(),
            strict_to_tag: config.strict_to_tag,
        }
    }

    // ── map bookkeeping (keeps the active-txn gauge == map sizes) ───────────

    /// The transaction `id` names.
    pub(super) fn txn(&self, id: TxnRef<'_>) -> Option<&Transaction> {
        match id {
            TxnRef::Client(branch) => self.clients.get(branch),
            TxnRef::Server(id) => self.servers.get(&id as &dyn ServerTxnIdentity),
        }
        .map(|t| &**t)
    }

    /// [`txn`](Self::txn), mutable.
    pub(super) fn txn_mut(&mut self, id: TxnRef<'_>) -> Option<&mut Transaction> {
        match id {
            TxnRef::Client(branch) => self.clients.get_mut(branch),
            TxnRef::Server(id) => self.servers.get_mut(&id as &dyn ServerTxnIdentity),
        }
        .map(|t| &mut **t)
    }

    /// The server transaction a message naming `id` belongs to (§17.2.3).
    pub(super) fn server(&self, id: ServerTxnId<'_>) -> Option<&Transaction> {
        self.txn(TxnRef::Server(id))
    }

    /// [`server`](Self::server), mutable.
    pub(super) fn server_mut(&mut self, id: ServerTxnId<'_>) -> Option<&mut Transaction> {
        self.txn_mut(TxnRef::Server(id))
    }

    pub(super) fn set_txn(&mut self, txn: Transaction) {
        let new_call_ref = txn.call_ref.clone();
        let id = txn.id.clone();
        // A re-insert of the same identity (a client send reusing a live
        // branch) must not double-count: drop the displaced txn's contribution
        // before adding the new one.
        let displaced = match &id {
            TxnId::Client(branch) => self.clients.insert(branch.clone(), Box::new(txn)),
            TxnId::Server(key) => self.servers.insert(key.clone(), Box::new(txn)),
        };
        if let Some(old) = displaced {
            // A request matching a server transaction is absorbed by it, and a
            // seed skips an occupied identity: only a client is displaced.
            debug_assert!(old.role() == TxnRole::Client, "a server transaction was displaced");
            // The displaced txn's queue entries are keyed by the SAME identity
            // the replacement now owns; left in the wheel they would fire
            // against the new txn (spurious retransmit/timeout/cleanup) and their
            // Keys would alias once their slots are reused. Physically remove them
            // now — cancel and queue membership move together (CLAUDE.md).
            self.cancel_timer(old.retransmit_key);
            self.cancel_timer(old.timeout_key);
            self.cancel_timer(old.cleanup_key);
            self.cancel_timer(old.cancel_grace_key);
            self.cancel_timer(old.cancel_retransmit_key);
            self.untrack_call_ref(&old.call_ref, id.as_ref());
            // The displaced txn's held CANCEL dies with it — a never-sent one
            // is counted dropped so the held counters reconcile
            // (held == flushed + flushed_pre1xx + dropped).
            if old.held_cancel.is_some_and(|h| h.wire == CancelWire::Held) {
                self.metrics
                    .held_cancels_dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.track_call_ref(&new_call_ref, &id);
        self.sync_active();
    }

    /// Add `id` to `call_ref`'s live transactions, in lockstep with a map
    /// insert. A txn with no `call_ref` (out-of-dialog initial INVITE /
    /// OPTIONS) is not indexed.
    fn track_call_ref(&mut self, call_ref: &Option<String>, id: &TxnId) {
        let Some(cr) = call_ref else { return };
        let txns = self.txn_index.entry(cr.clone()).or_default();
        match id {
            TxnId::Client(branch) => txns.client.insert(branch.clone()),
            TxnId::Server(key) => txns.server.insert(key.clone()),
        };
    }

    /// Remove `id` from `call_ref`'s live transactions, in lockstep with a
    /// map remove; the entry is dropped when it empties so `has_txns_for` is a
    /// plain `contains_key`.
    fn untrack_call_ref(&mut self, call_ref: &Option<String>, id: TxnRef<'_>) {
        let Some(cr) = call_ref else { return };
        let Some(txns) = self.txn_index.get_mut(cr) else { return };
        match id {
            TxnRef::Client(branch) => txns.client.remove(branch),
            TxnRef::Server(id) => txns.server.remove(&id as &dyn ServerTxnIdentity),
        };
        if txns.is_empty() {
            self.txn_index.remove(cr);
        }
    }

    /// Take the txn `id` names off its call's books while it stays
    /// resident: `has_txns_for` / `ActiveTxnCount` no longer see it, and a
    /// watched call it was the last transaction of hears `CallQuiesced` at end
    /// of turn (ADR-0014), exactly as if the txn had been deleted.
    pub(super) fn detach_from_call(&mut self, id: TxnRef<'_>) {
        let Some(cr) = self.txn_mut(id).and_then(|t| t.call_ref.take()) else {
            return;
        };
        self.untrack_call_ref(&Some(cr.clone()), id);
        if self.self_release_watch.contains(&cr) && !self.has_txns_for(&cr) {
            self.pending_quiesce.push(cr);
        }
    }

    pub(super) fn delete_txn(&mut self, id: TxnRef<'_>) -> bool {
        let removed = match id {
            TxnRef::Client(branch) => self.clients.remove(branch),
            TxnRef::Server(id) => self.servers.remove(&id as &dyn ServerTxnIdentity),
        };
        match removed {
            Some(t) => {
                if t.role() == TxnRole::Server {
                    if let Some(tag) = t.bound_to_tag() {
                        self.remember_uas_tag(&t.call_id, &t.from_tag, tag);
                    }
                }
                self.cancel_timer(t.retransmit_key);
                self.cancel_timer(t.timeout_key);
                self.cancel_timer(t.cleanup_key);
                self.cancel_timer(t.cancel_grace_key);
                self.cancel_timer(t.cancel_retransmit_key);
                self.untrack_call_ref(&t.call_ref, id);
                if t.held {
                    self.release_hold(t.branch(), &t.call_id, &t.from_tag);
                }
                self.sync_active();
                if t.orphaned {
                    self.metrics
                        .orphaned_transactions
                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
                // A txn leaving with a never-sent CANCEL: the crossing-2xx
                // final (cancellation moot), an
                // INVITE that gave up unanswered (its CANCEL waits for a
                // provisional, §9.1, to the end of the give-up hold), any
                // INVITE under the strict §9.1 policy, and the sweep. Under
                // the bounded policy grace expiry, evict and timeout of an
                // active INVITE send it first. Counted dropped here; a
                // grace-sent copy is already accounted.
                if t.held_cancel.as_ref().is_some_and(|h| h.wire == CancelWire::Held) {
                    self.metrics
                        .held_cancels_dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                // ADR-0014 self-release: if this was the LAST transaction for a
                // watched call, the consumer must hear CallQuiesced so it can shed
                // its acting-backup takeover copy — but only AFTER this turn's
                // protocol event (the ACK/Timeout that drove the delete), so defer
                // it to `flush_pending_quiesce`. (For a 2xx INVITE the server txn
                // lingers in `Completed` until Timer H — the ACK reuses a different
                // branch — so this naturally fires at Timer H, after the ACK relay.
                // A non-2xx INVITE server txn detached on its ACK, `call_ref`
                // already `None`, was accounted then.)
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

    pub(super) fn cancel_timer(&mut self, key: Option<Key>) {
        if let Some(k) = key {
            self.timers.try_remove(&k);
        }
    }

    fn sync_active(&self) {
        self.metrics
            .active_transactions
            .store(self.clients.len() + self.servers.len(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Any transaction (any role/state) still attributed to `call_ref`? O(1) via
    /// the lockstep `txn_index` (an empty branch set has no entry).
    pub(super) fn has_txns_for(&self, call_ref: &str) -> bool {
        self.txn_index.contains_key(call_ref)
    }

    // ── output ──────────────────────────────────────────────────────────────

    pub(super) async fn send_buffer(
        &self,
        endpoint: &dyn UdpEndpoint,
        buf: &[u8],
        dest: SocketAddr,
    ) {
        self.metrics.count_send(endpoint.send_to(buf, dest).await);
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
                if let Some(t) = self.clients.get_mut(&branch) {
                    t.retransmit_key = None;
                }
                self.fire_retransmit(endpoint, &branch).await
            }
            Timer::ServerRetransmit(key) => {
                if let Some(t) = self.server_mut(key.id()) {
                    t.retransmit_key = None;
                }
                self.fire_server_retransmit(endpoint, key.id()).await
            }
            Timer::ClientTimeout(branch) => {
                if let Some(t) = self.clients.get_mut(&branch) {
                    t.timeout_key = None;
                }
                self.fire_timeout(endpoint, &branch).await
            }
            Timer::Cleanup(id) => {
                if let Some(t) = self.txn_mut(id.as_ref()) {
                    t.cleanup_key = None;
                }
                self.expire(id.as_ref());
            }
            Timer::CancelGrace(branch) => {
                if let Some(t) = self.clients.get_mut(&branch) {
                    t.cancel_grace_key = None;
                }
                self.fire_cancel_grace(endpoint, &branch).await;
            }
            Timer::CancelRetransmit(branch) => {
                if let Some(t) = self.clients.get_mut(&branch) {
                    t.cancel_retransmit_key = None;
                }
                self.fire_cancel_retransmit(endpoint, &branch).await;
            }
            Timer::EventRetry => {
                self.event_retry_armed = false;
                self.flush_deferred();
            }
        }
    }

    /// The periodic housekeeping pass: the lifetime safety net, the Timer L
    /// memo purge, and the idle-table shrink and buffer census.
    fn sweep(&mut self) {
        self.reap_overdue();
        self.forget_expired_uas_tags();
        shrink_idle(&mut self.clients);
        shrink_idle(&mut self.servers);
        shrink_idle(&mut self.txn_index);
        // `shrink_to_fit` only trims the slab's free tail and never moves an
        // entry, so every `Key` stored on a txn stays valid (`compact` would not).
        let cap = self.timers.capacity();
        if cap > SHRINK_MIN_CAPACITY && self.timers.len() < cap / 4 {
            self.timers.shrink_to_fit();
        }
        // Census the retained retransmit-buffer bytes (same periodic pass) so a
        // buffer-retention leak is visible vs flat txns.
        let buf_bytes: u64 = self
            .clients
            .values()
            .chain(self.servers.values())
            .map(|t| t.retransmit_buf.as_ref().map_or(0, |b| b.len()) as u64)
            .sum();
        self.metrics.retransmit_buf_bytes.store(buf_bytes, std::sync::atomic::Ordering::Relaxed);
    }

    // ── send API command handling ─────────────────────────────────────────────

    async fn handle_command(&mut self, endpoint: &dyn UdpEndpoint, cmd: Command) {
        match cmd {
            Command::SendRequest { msg, dest, txn_type, reply } => {
                let handle = self.do_send_request(endpoint, *msg, dest, txn_type).await;
                let _ = reply.send(handle);
            }
            Command::SendResponse { msg, dest, reply } => {
                let sent = self.do_send_response(endpoint, *msg, dest).await;
                let _ = reply.send(sent);
            }
            Command::SendRaw { buf, dest, reply } => {
                // Bypasses transaction management; still counts a send failure.
                self.metrics.count_send(endpoint.send_to(&buf, dest).await);
                let _ = reply.send(());
            }
            Command::Seed { call_ref, seeds, reply } => {
                let seeded = self.do_seed(&call_ref, seeds);
                let _ = reply.send(seeded);
            }
            Command::Reoffer { message, src, reply } => {
                let disposition = self.do_reoffer(endpoint, *message, src).await;
                let _ = reply.send(disposition);
            }
            Command::CancelTxnsForCall { call_ref, reply } => {
                self.do_cancel_txns_for_call(endpoint, &call_ref).await;
                let _ = reply.send(());
            }
            Command::ForgetUnanswered { key, call_id, from_tag } => {
                self.forget_unanswered(&key, &call_id, &from_tag)
            }
            Command::ForgetUnansweredOfCall { call_ref, reply } => {
                let _ = reply.send(self.forget_unanswered_of_call(&call_ref));
            }
            Command::AnswerUnansweredInvitesOfCall { call_ref, status, reason, reply } => {
                let answered = self
                    .answer_unanswered_invites_of_call(endpoint, &call_ref, status, &reason)
                    .await;
                let _ = reply.send(answered);
            }
            Command::ActiveTxnCount { call_ref, reply } => {
                let n = self.txn_index.get(call_ref.as_str()).map_or(0, CallTxns::len);
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

    // ── inbound packet dispatch ───────────────────────────────────────────────

    async fn handle_packet(&mut self, endpoint: &dyn UdpEndpoint, packet: sip_net::UdpPacket) {
        use std::sync::atomic::Ordering::Relaxed;
        let src = packet.src;
        // Hand the receive buffer to the parser instead of lending it: the
        // message's `raw`/`body` then share it and no packet byte is copied.
        let parsed = match self.parser.parse_shared(bytes::Bytes::from(packet.raw)) {
            Ok(m) => m,
            Err(_e) => {
                // Parse error: the datagram is dropped. Count it so a
                // malformed-traffic flood / parser regression is visible.
                self.metrics.parse_errors.fetch_add(1, Relaxed);
                return;
            }
        };

        match parsed {
            SipMessage::Request(req) => self.handle_inbound_request(endpoint, req, src).await,
            SipMessage::Response(resp) => self.handle_inbound_response(endpoint, resp, src).await,
        }
    }
}

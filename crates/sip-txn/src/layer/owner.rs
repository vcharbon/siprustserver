//! The owner task's state: transaction-map + timer-wheel bookkeeping (kept in
//! lockstep), timer dispatch, and the periodic sweep tick; its `select!` loop
//! is `run`, its inbound-packet parsing `ingress`. Protocol behavior does NOT live here — the UAC FSM is
//! `layer::client`, the UAS FSM is `layer::server`, output-queue discipline is
//! `layer::events`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_message::SipParser;
use sip_net::UdpEndpoint;
use tokio::sync::mpsc;
use tokio_util::time::{delay_queue::Key, DelayQueue};

use crate::event::TransactionEvent;
use crate::metrics::MetricsInner;
use crate::rng::IdGen;

use super::backlog::DeferredBound;
use super::call_txns::CallTxns;
use super::handle::TransactionConfig;
use super::key::{ServerTxnId, ServerTxnIdentity, ServerTxnKey};
use super::parse_refusal::ParseRefusals;
use super::refusals::InviteRefusals;
use super::shrink::{shrink_idle, SHRINK_MIN_CAPACITY};
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
    /// The TU's first-response deadline on an initial INVITE
    /// ([`TransactionConfig::invite_first_response_timeout_ms`](crate::TransactionConfig)).
    pub(super) invite_first_response_timeout_ms: u64,
    /// [`TransactionConfig::trying_bound_ms`](crate::TransactionConfig).
    pub(super) trying_bound_ms: u64,
    /// [`TransactionConfig::ack_bound_ms`](crate::TransactionConfig).
    pub(super) ack_bound_ms: u64,
    /// The To-tag this node bound to a dialog it answered as UAS, by
    /// (Call-ID, From-tag), kept for Timer L past the transaction so a
    /// CANCEL or a stray answered after the transaction is gone still carries
    /// it (RFC 3261 §9.2, §12.1.1). Purged on the sweep tick
    /// (`forget_expired_uas_tags`).
    pub(super) recent_uas_tags: HashMap<(String, String), (String, tokio::time::Instant)>,
    /// [`TransactionConfig::strict_to_tag`](crate::TransactionConfig).
    pub(super) strict_to_tag: bool,
    /// [`TransactionConfig::stateless_options`](crate::TransactionConfig).
    pub(super) stateless_options: bool,
    /// The cap and the To-tags of the 400s answering refused requests.
    pub(super) parse_refusals: ParseRefusals,
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
        let unanswerable = config.deferred_bound.is_some() && config.invite_refusals.is_none();
        debug_assert!(!unanswerable, "a deferred bound refuses through invite_refusals");
        if unanswerable {
            tracing::warn!("a deferred bound without invite_refusals refuses nothing");
        }
        Self {
            clients: HashMap::new(),
            servers: HashMap::new(),
            timers: DelayQueue::new(),
            parser,
            events_tx,
            metrics,
            parse_refusals: ParseRefusals::new(&config.id_gen, config.advertised),
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
            trying_bound_ms: config.trying_bound_ms,
            ack_bound_ms: config.ack_bound_ms,
            recent_uas_tags: HashMap::new(),
            strict_to_tag: config.strict_to_tag,
            stateless_options: config.stateless_options,
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
            self.cancel_timer(old.cancel_retransmit_key);
            self.untrack_call_ref(&old.call_ref, id.as_ref());
            // The displaced txn's held CANCEL dies with it — a never-sent one
            // is counted dropped so the held counters reconcile
            // (held == flushed + dropped).
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
            TxnRef::Client(branch) => txns.client.remove(branch) | txns.accepted.remove(branch),
            TxnRef::Server(id) => txns.server.remove(&id as &dyn ServerTxnIdentity),
        };
        if txns.holds_none() {
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
        self.quiesce_if_last(cr);
    }

    /// Take the client txn at `branch`, Accepted after a 2xx, off its call's
    /// books while the call keeps it: `has_txns_for` / `ActiveTxnCount` no
    /// longer see it, as [`detach_from_call`](Self::detach_from_call), but
    /// `cancel_txns_for_call` and `relinquish_call` still reach it.
    pub(super) fn settle_accepted(&mut self, branch: &str) {
        let Some(cr) = self.clients.get(branch).and_then(|t| t.call_ref.clone()) else {
            return;
        };
        let Some(txns) = self.txn_index.get_mut(&cr) else { return };
        if txns.client.remove(branch) {
            txns.accepted.insert(branch.to_string());
            self.quiesce_if_last(cr);
        }
    }

    /// A watched call left with no transaction on its books hears
    /// `CallQuiesced` at end of turn (ADR-0014).
    fn quiesce_if_last(&mut self, cr: String) {
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
                // A txn leaving with a never-sent CANCEL — the crossing-2xx
                // final (cancellation moot), an INVITE whose branch drew no
                // provisional to the end of its hold (§9.1), the sweep — is
                // counted dropped here.
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
                // lingers in `Accepted` until Timer L — the ACK of a 2xx takes a
                // new branch and matches no transaction — so this fires at Timer
                // L, after the ACK relay. An INVITE server txn detached on an ACK
                // it matched, `call_ref` already `None`, was accounted then, as
                // was a client txn settled Accepted on its 2xx.)
                if let Some(cr) = t.call_ref {
                    self.quiesce_if_last(cr);
                }
                true
            }
            None => false,
        }
    }

    /// Arm `timer` after `delay`, held within the wheel's range.
    pub(super) fn arm(&mut self, timer: Timer, delay: Duration) -> Key {
        sip_clock::delay_queue::insert(&mut self.timers, timer, delay)
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

    /// Any transaction (any role/state) still on `call_ref`'s books? O(1) via
    /// the lockstep `txn_index`; an Accepted client txn is not on them.
    pub(super) fn has_txns_for(&self, call_ref: &str) -> bool {
        self.txn_index.get(call_ref).is_some_and(|t| t.on_books() > 0)
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

    pub(super) async fn fire_timer(&mut self, endpoint: &dyn UdpEndpoint, timer: Timer) {
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
                self.fire_timeout(&branch)
            }
            Timer::FirstResponse(branch) => self.fire_first_response(&branch),
            Timer::Trying(key, opened) => self.fire_trying(endpoint, &key, opened).await,
            Timer::AckBound(branch, opened) => self.fire_ack_bound(endpoint, &branch, opened).await,
            Timer::Cleanup(id) => {
                if let Some(t) = self.txn_mut(id.as_ref()) {
                    t.cleanup_key = None;
                }
                self.expire(id.as_ref());
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
    pub(super) fn sweep(&mut self) {
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
        // buffer-retention leak is visible vs flat txns: the request images
        // and the ACKs an Accepted client txn keeps for its 2xx repeats.
        let buf_bytes: u64 = self
            .clients
            .values()
            .chain(self.servers.values())
            .map(|t| {
                let acks = t.dialogs.iter().filter_map(|d| d.ack.as_ref().map(|(ack, _)| ack));
                t.retransmit_buf.iter().chain(acks).map(|b| b.len() as u64).sum::<u64>()
            })
            .sum();
        self.metrics.retransmit_buf_bytes.store(buf_bytes, std::sync::atomic::Ordering::Relaxed);
    }
}

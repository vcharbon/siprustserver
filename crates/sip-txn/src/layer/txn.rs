//! Per-transaction state: the [`Transaction`] record, its role/state enums,
//! the [`Timer`] wheel entries, and the sweep-age policy. The FSM transitions
//! do NOT live here — client (§17.1) side is `layer::client`, server (§17.2)
//! side is `layer::server`.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::{Method, SipRequest};
use tokio_util::time::delay_queue::Key;

use crate::event::{TimeoutKind, TxnKind};
use crate::timers::{ms, TXN_MAX_AGE};
use sip_retransmit::Ladder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TxnRole {
    Client,
    Server,
}

/// Transaction lifecycle. A final response deletes the transaction outright
/// (after its Timer D/H/J hold) rather than parking it in a `Terminated`
/// state, so `Completed` is the last state a resident txn can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TxnState {
    Trying,
    Proceeding,
    Completed,
}

impl TxnState {
    /// Still timing/retransmitting — Timer B/F and retransmits act only here.
    pub(super) fn is_active(self) -> bool {
        matches!(self, TxnState::Trying | TxnState::Proceeding)
    }
}

/// Which DelayQueue entry a fired timer corresponds to (keyed by branch).
#[derive(Debug, Clone)]
pub(super) enum Timer {
    /// Timer A (INVITE) / Timer E (non-INVITE) — client retransmit.
    ClientRetransmit(String),
    /// Timer G (RFC 3261 §17.2.1) — INVITE *server* txn retransmit of an unACKed
    /// non-2xx final. Disjoint from `ClientRetransmit` (a txn is client XOR server),
    /// so both reuse the `retransmit_*` fields without colliding.
    ServerRetransmit(String),
    /// Timer B (INVITE) / Timer F (non-INVITE) — client transaction timeout.
    ClientTimeout(String),
    /// Delete-by-branch cleanup: Timer H/J/Timer-H-487 (server final-response hold)
    /// AND Timer D (client non-2xx-final hold, §17.1.1.2). The fire handler just
    /// `delete_txn`s the branch, so it serves both roles.
    Cleanup(String),
    /// Held-CANCEL grace expiry (ADR-0028): the branch's first provisional never
    /// arrived inside the grace window, so the held CANCEL is sent regardless.
    CancelGrace(String),
    /// Timer E for the on-wire CANCEL (RFC 3261 §17.1.2.2 — a CANCEL is a
    /// non-INVITE request, but it reuses its INVITE's branch and builds no txn
    /// of its own, so its retransmit ladder rides the INVITE client txn as the
    /// [`HeldCancel`] sub-state). Paced T1 → doubling → capped at T2, stopped
    /// at the 64·T1 ceiling.
    CancelRetransmit(String),
    /// Re-offer CRITICAL events (Timeout / Cancelled / CallQuiesced / a consumed
    /// non-2xx final / an inbound INVITE whose 100 silenced the UAC) that a full
    /// events queue deferred — never dropped. At most ONE in flight
    /// (`event_retry_armed`); its `Key` is never stored, so the CLAUDE.md
    /// stale-`Key` aliasing hazard cannot arise.
    EventRetry,
}

/// Wire status of the CANCEL datagram parked on its INVITE client txn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancelWire {
    /// Never sent — waiting on the branch's first provisional or the grace
    /// expiry (RFC 3261 §9.1, bounded per ADR-0028).
    Held,
    /// On the wire pre-1xx (grace/evict/timeout flush); ONE re-send on the
    /// branch's late first provisional is still owed — a UAS that 481'd the
    /// pre-1xx copy has built its server txn by then. Not "never sent": a
    /// dying txn does not count it dropped.
    SentPre1xx,
    /// On the wire with nothing further owed on a provisional; only the
    /// [`Timer::CancelRetransmit`] ladder re-sends it.
    Sent,
}

/// A CANCEL datagram parked on its INVITE client txn (see
/// [`Transaction::held_cancel`]).
pub(super) struct HeldCancel {
    pub(super) buf: Bytes,
    pub(super) dest: SocketAddr,
    pub(super) wire: CancelWire,
    /// Timer-E pacing for the on-wire datagram (§17.1.2.2). `None` while the
    /// datagram is still [`CancelWire::Held`]; armed, and re-armed from rung
    /// one, by [`super::owner::Owner`]'s `arm_cancel_retransmit`.
    pub(super) ladder: Option<Ladder>,
}

impl HeldCancel {
    pub(super) fn new(buf: Bytes, dest: SocketAddr, wire: CancelWire) -> Self {
        Self {
            buf,
            dest,
            wire,
            ladder: None,
        }
    }
}

/// What distinguishes one fresh transaction from another: its identity, its
/// attribution and the state it is born in. Everything else on a
/// [`Transaction`] — cached responses, timer keys, the held CANCEL, the ladder
/// — starts empty and is written by the path that arms it.
pub(super) struct NewTransaction {
    pub(super) branch: String,
    pub(super) role: TxnRole,
    pub(super) kind: TxnKind,
    pub(super) method: Method,
    pub(super) call_id: String,
    pub(super) from_tag: String,
    pub(super) original_request: Option<SipRequest>,
    pub(super) call_ref: Option<String>,
    pub(super) leg_id: Option<String>,
    pub(super) state: TxnState,
    pub(super) destination: Option<SocketAddr>,
}

impl Transaction {
    /// The one constructor every path that puts a transaction in the map uses
    /// — a send, an admitted request, a seed — so the record's shape is written
    /// once.
    pub(super) fn new(head: NewTransaction) -> Self {
        Self {
            branch: head.branch,
            role: head.role,
            kind: head.kind,
            method: head.method,
            call_id: head.call_id,
            from_tag: head.from_tag,
            original_request: head.original_request,
            last_response: None,
            last_response_status: None,
            call_ref: head.call_ref,
            leg_id: head.leg_id,
            state: head.state,
            destination: head.destination,
            created_at: tokio::time::Instant::now(),
            uas_to_tag: None,
            retransmit_key: None,
            timeout_key: None,
            cleanup_key: None,
            held_cancel: None,
            cancel_grace_key: None,
            cancel_retransmit_key: None,
            retransmit_buf: None,
            ladder: None,
            timeout_kind: TimeoutKind::Response,
        }
    }
}

pub(super) struct Transaction {
    pub(super) branch: String,
    pub(super) role: TxnRole,
    pub(super) kind: TxnKind,
    /// The method of the request this transaction is for — the CSeq method
    /// of every message on it, and the `method` a rung of its ladder is
    /// counted under.
    pub(super) method: Method,
    pub(super) call_id: String,
    pub(super) from_tag: String,
    pub(super) original_request: Option<SipRequest>,
    /// The datagram replayed on a request retransmit. Refcounted, so caching a
    /// freshly built message keeps its rendered image instead of copying it, and
    /// a replay costs a refcount bump rather than a memcpy of the whole message.
    pub(super) last_response: Option<Bytes>,
    pub(super) last_response_status: Option<u16>,
    pub(super) call_ref: Option<String>,
    pub(super) leg_id: Option<String>,
    pub(super) state: TxnState,
    pub(super) destination: Option<SocketAddr>,
    pub(super) created_at: tokio::time::Instant,
    /// UAS To-tag pinned on the first >100 response (RFC 3261 §17.2.1).
    pub(super) uas_to_tag: Option<String>,
    // DelayQueue keys so a txn's timers cancel in O(1).
    pub(super) retransmit_key: Option<Key>,
    pub(super) timeout_key: Option<Key>,
    pub(super) cleanup_key: Option<Key>,
    /// A CANCEL datagram held back because this INVITE client txn has received
    /// no response yet (RFC 3261 §9.1 — the CANCEL waits for the first
    /// provisional). Flushed on the first 1xx, or sent regardless when the
    /// grace window expires (ADR-0028: the wait is a courtesy, never a veto —
    /// every emitted CANCEL reaches the wire). Cleared without sending only
    /// when the txn takes a final first (the UAS already answered — §9.2).
    /// Only ever set on a `Client`/`Invite` txn in `Trying`.
    pub(super) held_cancel: Option<HeldCancel>,
    /// The [`Timer::CancelGrace`] wheel entry for a held CANCEL — cancelled in
    /// lockstep whenever the hold resolves (flush/drop/txn death), nulled at
    /// fire (the CLAUDE.md stale-`Key` discipline).
    pub(super) cancel_grace_key: Option<Key>,
    /// The [`Timer::CancelRetransmit`] wheel entry for an on-wire CANCEL —
    /// cancelled in lockstep with the sub-state's resolution (a CANCEL
    /// response, the INVITE's final, txn death), nulled at fire.
    pub(super) cancel_retransmit_key: Option<Key>,
    // Retransmit progression.
    /// The request datagram Timer A/E re-sends — refcounted for the same reason
    /// as [`last_response`](Self::last_response).
    pub(super) retransmit_buf: Option<Bytes>,
    /// The Timer A/E ladder for [`retransmit_buf`](Self::retransmit_buf).
    /// `None` until this txn starts retransmitting, and on a server txn until
    /// its non-2xx final arms Timer G.
    pub(super) ladder: Option<Ladder>,
    /// Which [`TimeoutKind`] `fire_timeout` emits for this txn's give-up timer —
    /// stored explicitly wherever the timer is armed (`Response` for Timer B/F,
    /// `Transaction` once the first provisional swaps in the long out-of-dialog
    /// INVITE bound) so the discrimination never compares the armed window
    /// against a magic duration.
    pub(super) timeout_kind: TimeoutKind,
}

/// Per-txn safety-net age for the sweep. A still-ringing INVITE (no final
/// response yet — an inbound INVITE awaiting the app's answer, or an outbound
/// INVITE past its retransmit window) legitimately outlives the 35 s net: a
/// callee may ring for minutes and the no-answer timer / the configured
/// initial-INVITE bound owns that deadline. Give it a backstop just above
/// `invite_initial_timeout_ms` (the configured bound — BOTH roles: it is the
/// ONLY pre-final bound on an INVITE server txn, so the a-leg admits the same
/// ring window the b-leg client txn does) so the net never reaps a live call.
/// Everything else — completed txns governed by Timer H/J, all non-INVITE —
/// keeps the tight 35 s net just above 32 s, so the sweep still only ever
/// catches what a missing-cleanup bug would otherwise leak.
pub(super) fn sweep_max_age(
    t: &Transaction,
    invite_initial_timeout_ms: u64,
) -> std::time::Duration {
    match (t.kind, t.state) {
        (TxnKind::Invite, TxnState::Trying | TxnState::Proceeding) => {
            ms(invite_initial_timeout_ms + TXN_MAX_AGE)
        }
        _ => ms(TXN_MAX_AGE),
    }
}

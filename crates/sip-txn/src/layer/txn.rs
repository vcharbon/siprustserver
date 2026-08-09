//! Per-transaction state: the [`Transaction`] record, its role/state enums,
//! the [`Timer`] wheel entries, and the sweep-age policy. The FSM transitions
//! do NOT live here — client (§17.1) side is `layer::client`, server (§17.2)
//! side is `layer::server`.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::SipRequest;
use tokio_util::time::delay_queue::Key;

use crate::event::TxnKind;
use crate::timers::{ms, INVITE_INITIAL_TIMEOUT, TXN_MAX_AGE};

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
    /// Re-offer CRITICAL events (Timeout / Cancelled / CallQuiesced / a consumed
    /// non-2xx final / an inbound INVITE whose 100 silenced the UAC) that a full
    /// events queue deferred — never dropped. At most ONE in flight
    /// (`event_retry_armed`); its `Key` is never stored, so the CLAUDE.md
    /// stale-`Key` aliasing hazard cannot arise.
    EventRetry,
}

pub(super) struct Transaction {
    pub(super) branch: String,
    pub(super) role: TxnRole,
    pub(super) kind: TxnKind,
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
    /// no response yet (RFC 3261 §9.1 — the CANCEL MUST wait for the first
    /// provisional). Flushed on the first 1xx; dropped when the txn takes a
    /// final or dies at Timer B (no CANCEL is owed to a dead transaction).
    /// Only ever set on a `Client`/`Invite` txn in `Trying`.
    pub(super) held_cancel: Option<(Bytes, SocketAddr)>,
    // Retransmit progression.
    /// The request datagram Timer A/E re-sends — refcounted for the same reason
    /// as [`last_response`](Self::last_response).
    pub(super) retransmit_buf: Option<Bytes>,
    pub(super) retransmit_interval_ms: u64,
    pub(super) retransmit_elapsed_ms: u64,
    pub(super) retransmit_max_ms: u64,
}

/// Per-txn safety-net age for the sweep. A still-ringing INVITE (no final
/// response yet — an inbound INVITE awaiting the app's answer, or an outbound
/// INVITE past its retransmit window) legitimately outlives the 35 s net: a
/// callee may ring for minutes and the no-answer timer / long initial-INVITE
/// Timer B owns that deadline. Give it a backstop just above that long timeout so
/// the net never reaps a live call. Everything else — completed txns governed by
/// Timer H/J, all non-INVITE — keeps the tight 35 s net just above 32 s, so the
/// sweep still only ever catches what a missing-cleanup bug would otherwise leak.
pub(super) fn sweep_max_age(t: &Transaction) -> std::time::Duration {
    match (t.kind, t.state) {
        (TxnKind::Invite, TxnState::Trying | TxnState::Proceeding) => {
            ms(INVITE_INITIAL_TIMEOUT + TXN_MAX_AGE)
        }
        _ => ms(TXN_MAX_AGE),
    }
}

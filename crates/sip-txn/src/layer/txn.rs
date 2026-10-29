//! Per-transaction state: the [`Transaction`] record, its role/state enums
//! and the [`Timer`] wheel entries. The FSM transitions do NOT live here —
//! client (§17.1) side is `layer::client`, server (§17.2) side is
//! `layer::server`; when a transaction leaves the map is `layer::lifetime`.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::{Method, SipRequest};
use tokio_util::time::delay_queue::Key;

use call::{TxnKind, TxnTimeoutKind};
use sip_retransmit::Ladder;

use super::key::{ServerTxnId, ServerTxnKey};
use super::lifetime::Lifetime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TxnRole {
    Client,
    Server,
}

/// Transaction lifecycle. There is no `Terminated` state: the hold that
/// follows a final (Timer D/H/J/L/M, or Timer I after the ACK) ends by
/// deleting the transaction from the map ([`Lifetime`]). A resident txn that
/// has sent or received its final is `Completed`, `Confirmed` or `Accepted`;
/// each refuses a second final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TxnState {
    Trying,
    Proceeding,
    /// A final sent or received; held for Timer D/H/J.
    Completed,
    /// INVITE server txn whose non-2xx final is ACKed (RFC 3261 §17.2.1):
    /// held for Timer I, absorbing ACK retransmissions and any further final
    /// the TU offers on the branch. Nothing retransmits or times out here.
    Confirmed,
    /// RFC 6026 Accepted, after a 2xx; nothing retransmits or times out here.
    /// An INVITE server txn that sent a 2xx (§7.1, §8.7): held for Timer L,
    /// passing every ACK up and handing no copy of its INVITE up; this stack
    /// answers each copy with that 2xx where §8.7 sends nothing (`replay_cached`).
    /// An INVITE client txn that took a 2xx (§7.2): held for Timer M from
    /// that 2xx, absorbing every 1xx and non-2xx (§8.4). While its call holds
    /// it, every later 2xx reaches the TU, which owns each ACK (RFC 3261
    /// §13.2.2.4); once orphaned, the layer sends each dialog's ACK itself
    /// (`layer::orphan`).
    Accepted,
}

impl TxnState {
    /// No final yet: Timer B/F and retransmits act only here, and a client
    /// INVITE that gave up stays here, held, with both stopped.
    pub(super) fn is_active(self) -> bool {
        matches!(self, TxnState::Trying | TxnState::Proceeding)
    }
}

/// A transaction's identity, owned: a client transaction's branch, a server
/// transaction's [`ServerTxnKey`] (RFC 3261 §17.1.3, §17.2.3).
#[derive(Debug, Clone)]
pub(super) enum TxnId {
    Client(String),
    Server(ServerTxnKey),
}

impl TxnId {
    pub(super) fn as_ref(&self) -> TxnRef<'_> {
        match self {
            TxnId::Client(branch) => TxnRef::Client(branch),
            TxnId::Server(key) => TxnRef::Server(key.id()),
        }
    }
}

/// A transaction's identity, borrowed — from a message, a timer or the
/// transaction itself.
#[derive(Debug, Clone, Copy)]
pub(super) enum TxnRef<'a> {
    Client(&'a str),
    Server(ServerTxnId<'a>),
}

impl TxnRef<'_> {
    pub(super) fn role(self) -> TxnRole {
        match self {
            TxnRef::Client(_) => TxnRole::Client,
            TxnRef::Server(_) => TxnRole::Server,
        }
    }

    pub(super) fn to_id(self) -> TxnId {
        match self {
            TxnRef::Client(branch) => TxnId::Client(branch.to_string()),
            TxnRef::Server(id) => TxnId::Server(id.to_key()),
        }
    }
}

/// Which DelayQueue entry a fired timer corresponds to: the transaction it
/// belongs to, a client one by its branch, a server one by its key.
#[derive(Debug, Clone)]
pub(super) enum Timer {
    /// Timer A (INVITE) / Timer E (non-INVITE) — client retransmit.
    ClientRetransmit(String),
    /// Timer G (RFC 3261 §17.2.1) — INVITE *server* txn retransmit of an unACKed
    /// non-2xx final. Disjoint from `ClientRetransmit` (a txn is client XOR server),
    /// so both reuse the `retransmit_*` fields without colliding.
    ServerRetransmit(ServerTxnKey),
    /// Timer B (INVITE) / Timer F (non-INVITE) — client transaction timeout.
    ClientTimeout(String),
    /// The TU's first-response deadline on an initial INVITE (ADR-0044): fired
    /// with the branch still unanswered, it reports the hop dead; the
    /// transaction runs on to Timer B. Its key is never stored — the fire
    /// reads the transaction's state and is a no-op on anything else.
    FirstResponse(String),
    /// The §17.2.1 bound of the server INVITE transaction opened at the
    /// instant it carries: its 100 leaves then if nothing has (`layer::trying`).
    /// Its key is never stored; the fire reads the transaction's state.
    Trying(ServerTxnKey, tokio::time::Instant),
    /// The transaction's end of life, due at its [`Lifetime`] deadline — the
    /// backstop of an active one, the end of a hold otherwise. Every resident
    /// transaction has exactly one; firing removes the transaction.
    Cleanup(TxnId),
    /// The bound of the client INVITE transaction opened at the instant it
    /// carries on the ACK of its non-2xx final (`layer::held_ack`): the ACK
    /// leaves then if its consumer released none. Its key is never stored;
    /// the fire reads the transaction.
    AckBound(String, tokio::time::Instant),
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
    /// Never sent — waiting on the branch's first provisional (RFC 3261
    /// §9.1).
    Held,
    /// On the wire; only the [`Timer::CancelRetransmit`] ladder re-sends it.
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
        Self { buf, dest, wire, ladder: None }
    }
}

/// The most dialogs, and reliably offered To-tags, an INVITE client txn
/// remembers: one per fork (RFC 6026 §7.2), bounded against a peer that
/// invents To-tags.
pub(super) const MAX_DIALOGS: usize = 16;

/// A dialog of an INVITE client txn, by its To-tag (one per fork, RFC 3261
/// §12.1.2): the highest CSeq this layer sent on it, so a BYE it sends there
/// numbers above it (§12.2.1.1); whether a BYE already left on it; and the
/// ACK of its 2xx once one left, with where it went — the TU's, seen leaving
/// through the layer, or the one the layer built for an orphan (§13.2.2.4).
/// A dialog whose ACK left was held by the TU, or already released by the
/// layer; one a BYE left on is ended.
pub(super) struct InviteDialog {
    pub(super) to_tag: Option<String>,
    pub(super) cseq: u32,
    pub(super) byed: bool,
    pub(super) ack: Option<(Bytes, SocketAddr)>,
}

/// What distinguishes one fresh transaction from another: its identity, its
/// attribution and the state it is born in. Everything else on a
/// [`Transaction`] — cached responses, timer keys, the held CANCEL, the ladder
/// — starts empty and is written by the path that arms it.
pub(super) struct NewTransaction {
    pub(super) id: TxnId,
    pub(super) kind: TxnKind,
    pub(super) method: Method,
    /// The CSeq number of the request this transaction is for.
    pub(super) cseq: u32,
    pub(super) call_id: String,
    pub(super) from_tag: String,
    pub(super) original_request: Option<SipRequest>,
    pub(super) call_ref: Option<String>,
    pub(super) leg_id: Option<String>,
    pub(super) incarnation_mark: Option<String>,
    pub(super) state: TxnState,
    pub(super) destination: Option<SocketAddr>,
}

impl Transaction {
    /// The one constructor every path that puts a transaction in the map uses
    /// — a send, an admitted request, a seed — so the record's shape is written
    /// once. `lifetime` comes from `layer::lifetime`, which arms it.
    pub(super) fn new(head: NewTransaction, lifetime: Lifetime) -> Self {
        Self {
            id: head.id,
            kind: head.kind,
            method: head.method,
            cseq: head.cseq,
            call_id: head.call_id,
            from_tag: head.from_tag,
            original_request: head.original_request.map(Box::new),
            last_response: None,
            last_response_status: None,
            call_ref: head.call_ref,
            leg_id: head.leg_id,
            incarnation_mark: head.incarnation_mark,
            state: head.state,
            destination: head.destination,
            created_at: tokio::time::Instant::now(),
            uas_to_tag: None,
            final_to_tag: None,
            retransmit_key: None,
            timeout_key: None,
            cleanup_key: None,
            held_cancel: None,
            cancel_retransmit_key: None,
            retransmit_buf: None,
            seed_resend: None,
            unsent_ack: None,
            ladder: None,
            timeout_kind: TxnTimeoutKind::Response,
            dead_hop_reported: false,
            past_trying: false,
            dialogs: Vec::new(),
            reliable_offers: Vec::new(),
            lifetime,
            orphaned: false,
            held: false,
        }
    }

    /// The To-tag this server INVITE transaction has bound: its final's, else
    /// the one pinned on its first >100 response (RFC 3261 §8.2.6.2, §9.2,
    /// §12.1.1). A later early dialog mirrored from a forking downstream does
    /// not move it.
    pub(super) fn bound_to_tag(&self) -> Option<&str> {
        self.final_to_tag.as_deref().or(self.uas_to_tag.as_deref())
    }

    pub(super) fn role(&self) -> TxnRole {
        self.id.as_ref().role()
    }

    /// This INVITE's dialog under `to_tag`, recorded now when new; `None`
    /// for a new one past [`MAX_DIALOGS`].
    pub(super) fn dialog(&mut self, to_tag: Option<&str>) -> Option<&mut InviteDialog> {
        if let Some(at) = self.dialogs.iter().position(|d| d.to_tag.as_deref() == to_tag) {
            return self.dialogs.get_mut(at);
        }
        if self.dialogs.len() >= MAX_DIALOGS {
            return None;
        }
        let to_tag = to_tag.map(str::to_string);
        // One slot at a time: most INVITEs open one dialog, and a held txn's
        // spare capacity is memory held 64·T1.
        self.dialogs.reserve_exact(1);
        self.dialogs.push(InviteDialog { to_tag, cseq: 0, byed: false, ack: None });
        self.dialogs.last_mut()
    }

    /// A server transaction's key, owned; `None` for a client one.
    pub(super) fn server_key(&self) -> Option<ServerTxnKey> {
        match &self.id {
            TxnId::Server(key) => Some(key.clone()),
            TxnId::Client(_) => None,
        }
    }

    /// The top-Via branch of the request this transaction is for.
    pub(super) fn branch(&self) -> &str {
        match &self.id {
            TxnId::Client(branch) => branch,
            TxnId::Server(key) => key.branch(),
        }
    }
}

pub(super) struct Transaction {
    /// What this transaction is filed under (§17.1.3, §17.2.3).
    pub(super) id: TxnId,
    pub(super) kind: TxnKind,
    /// The method of the request this transaction is for — the CSeq method
    /// of every message on it, and the `method` a rung of its ladder is
    /// counted under.
    pub(super) method: Method,
    /// The CSeq number of the request this transaction is for: the ACK of each
    /// 2xx to an INVITE repeats it (RFC 3261 §13.2.2.4), a timeout names it.
    pub(super) cseq: u32,
    pub(super) call_id: String,
    pub(super) from_tag: String,
    /// The INVITE an INVITE transaction is for; an Accepted client one keeps
    /// its image instead ([`retransmit_buf`](Self::retransmit_buf)). Boxed: a
    /// parsed request is most of a record's size, and a non-INVITE has none.
    pub(super) original_request: Option<Box<SipRequest>>,
    /// The datagram replayed on a request retransmit. Refcounted, so caching a
    /// freshly built message keeps its rendered image instead of copying it, and
    /// a replay costs a refcount bump rather than a memcpy of the whole message.
    pub(super) last_response: Option<Bytes>,
    pub(super) last_response_status: Option<u16>,
    pub(super) call_ref: Option<String>,
    pub(super) leg_id: Option<String>,
    /// The `ci` mark of the call incarnation that sent a client request,
    /// read off its Via beside `cr` / `lg`; `None` for a server transaction.
    pub(super) incarnation_mark: Option<String>,
    pub(super) state: TxnState,
    /// Where it sends: a client transaction's request; every response of a
    /// server transaction, its request's response target (RFC 3261 §18.2.2)
    /// fixed when it opened.
    pub(super) destination: Option<SocketAddr>,
    /// The instant this transaction entered the map — its first send, its
    /// admission or its seed: the origin the client INVITE bound is measured
    /// from (`rearm_invite_bound`). When the transaction leaves is its
    /// [`lifetime`](Self::lifetime)'s alone.
    pub(super) created_at: tokio::time::Instant,
    /// UAS To-tag pinned on the first >100 response (RFC 3261 §17.2.1): the
    /// tag a CANCEL answer and the layer's 487 carry while no final has bound
    /// one (§8.2.6.2, §9.2).
    pub(super) uas_to_tag: Option<String>,
    /// The To-tag the final to this INVITE carried: the dialog's local tag,
    /// fixed from then on — every later response for this transaction or its
    /// CANCEL carries it (§9.2, §12.1.1).
    pub(super) final_to_tag: Option<String>,
    // DelayQueue keys so a txn's timers cancel in O(1).
    pub(super) retransmit_key: Option<Key>,
    pub(super) timeout_key: Option<Key>,
    /// The [`Timer::Cleanup`] at the [`lifetime`](Self::lifetime) deadline.
    pub(super) cleanup_key: Option<Key>,
    /// The CANCEL datagram parked on this INVITE client txn: held while the
    /// branch has drawn no response (RFC 3261 §9.1 — the CANCEL waits for the
    /// first provisional, however late) and flushed on the first 1xx, or on
    /// the wire riding its Timer-E ladder. A held one is cleared unsent when
    /// the txn takes a final first (§9.2) or dies.
    pub(super) held_cancel: Option<HeldCancel>,
    /// The [`Timer::CancelRetransmit`] wheel entry for an on-wire CANCEL —
    /// cancelled in lockstep with the sub-state's resolution (a CANCEL
    /// response, the INVITE's final, txn death), nulled at fire.
    pub(super) cancel_retransmit_key: Option<Key>,
    // Retransmit progression.
    /// The request datagram Timer A/E re-sends — refcounted for the same reason
    /// as [`last_response`](Self::last_response). On an Accepted INVITE client
    /// txn, the INVITE's image, which stands in for `original_request`.
    pub(super) retransmit_buf: Option<Bytes>,
    /// On a client INVITE seeded `Trying` (ADR-0014): its INVITE, re-sent once
    /// when a CANCEL is first held on it (ADR-0043), taken at that send.
    pub(super) seed_resend: Option<Bytes>,
    /// On a client INVITE that took a non-2xx final: its ACK and where it
    /// goes, held until the consumer releases it or its bound passes
    /// (`layer::held_ack`), taken at that send.
    pub(super) unsent_ack: Option<(Bytes, SocketAddr)>,
    /// The Timer A/E ladder for [`retransmit_buf`](Self::retransmit_buf).
    /// `None` until this txn starts retransmitting, and on a server txn until
    /// its non-2xx final arms Timer G.
    pub(super) ladder: Option<Ladder>,
    /// Which [`TxnTimeoutKind`] `fire_timeout` emits for this txn's give-up timer —
    /// stored explicitly wherever the timer is armed (`Response` for Timer B/F,
    /// `Transaction` once the first provisional swaps in the long out-of-dialog
    /// INVITE bound) so the discrimination never compares the armed window
    /// against a magic duration.
    pub(super) timeout_kind: TxnTimeoutKind,
    /// The TU was told at its first-response deadline that this initial
    /// INVITE's hop is dead (ADR-0044): no later timeout of the transaction is
    /// reported again.
    pub(super) dead_hop_reported: bool,
    /// On an INVITE client txn: a provisional above 100 answered it, so the
    /// TU holds a response and is never told the hop is alive.
    pub(super) past_trying: bool,
    /// On an INVITE client txn: the dialogs it opened that a request this
    /// layer sent (a PRACK, an UPDATE, a BYE) or a 2xx named, at most
    /// [`MAX_DIALOGS`]; a 2xx under another To-tag is a forked dialog of its
    /// own (RFC 6026 §7.2).
    pub(super) dialogs: Vec<InviteDialog>,
    /// On an INVITE client txn that carried no offer: the To-tags of the
    /// reliable provisionals that carried one, at most [`MAX_DIALOGS`]. Their
    /// PRACK answered it (RFC 3262 §5), so their 2xx's ACK owes no answer.
    pub(super) reliable_offers: Vec<String>,
    /// When this transaction leaves the map, written only by
    /// `layer::lifetime`. An INVITE client txn that gave up is held
    /// [`Hold::GaveUp`](super::lifetime::Hold::GaveUp) in Trying/Proceeding:
    /// its `Timeout` is delivered, a late final is ACKed and handed up
    /// (§17.1.1.3), it never times out again, and a CANCEL on it before any
    /// provisional stays held.
    pub(super) lifetime: Lifetime,
    /// The call this transaction served has been released while it was still
    /// open or Accepted (`cancel_txns_for_call`). The layer then closes the
    /// transaction's own obligations — the hop ACK of a non-2xx and Timer D,
    /// the ACK of each 2xx and Timer M, the held CANCEL, Timer B — and
    /// surfaces nothing to the consumer, which holds no state for it any more.
    pub(super) orphaned: bool,
    /// A server INVITE transaction — an admitted initial INVITE or a seed —
    /// whose identity the node's shared refusals hold while it lives, so no
    /// stage ahead of this layer refuses a copy of it; leaving the map
    /// releases the hold.
    pub(super) held: bool,
}

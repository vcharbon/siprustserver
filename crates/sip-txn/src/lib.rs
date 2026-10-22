//! sip-txn — the SIP transaction layer.
//!
//! RFC 3261 §17 client/server transaction state machines: duplicate detection
//! (§17.2.3: top-Via branch, sent-by and method), retransmission + timeout
//! timers (A/B for INVITE, E/F for non-INVITE, G non-2xx-final retransmit,
//! D/H/J holds, the RFC 6026 Accepted holds L and M), CANCEL→200+487, ACK
//! absorption for non-2xx, and cached-final-response retransmission.
//! In-memory only (≤ ~32 s txn lifetime past a final).
//!
//! ## Who ACKs a 2xx
//!
//! One owner per ACK (RFC 3261 §13.2.2.4, RFC 6026 §7.2): while a call holds
//! its Accepted INVITE client transaction, the TU, every later 2xx reaching it
//! as a repeat; once the call is released (`cancel_txns_for_call`), the layer,
//! re-sending the TU's ACK or building one from the INVITE (ADR-0034).
//!
//! Sits on a [`sip_net::UdpEndpoint`] (it parses raw datagrams itself, via a
//! [`sip_message::SipParser`]) and emits a stream of [`TransactionEvent`]s to
//! its consumer.
//!
//! ## What this crate is NOT
//!
//! Per-call FIFO dispatch (`SipRouter` + per-call executors) is a B2BUA-only
//! concern and lives there; the transaction layer is shared by the proxy and
//! the B2BUA. Its single-writer property is structural — one owner task over
//! the transaction map (see [`layer`] and docs/adr/0007, which also covers the
//! scalability rationale: flat `DelayQueue` memory vs. task-per-timer).

// The panic set at deny outside test code (ADR-0051 §3).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

pub mod catalogue;
pub mod event;
pub mod layer;
pub mod metrics;
pub mod parse_guard;
pub mod parse_refusal;
mod poison;
pub mod pulse;
pub mod rng;
pub mod seed;
mod stateless_tag;
pub mod timers;

// The layer actor: spawn, send API, per-call eviction, ADR-0014 self-release,
// the deferred-backlog ceiling on new calls and the node's shared refusals.
pub use layer::{
    DeferredBound, InviteClass, InviteRefusals, NewCallRefusal, ServerTxnKey, TransactionConfig,
    TransactionLayer, TransactionLayerClosed, Verdict, REFUSED_MEMO_MAX,
};
// The seeds a materialised call hands the layer, and a re-offer's disposition.
pub use seed::{Reoffer, TxnSeed};
// Upward events + the client-transaction handles `send_request` returns.
pub use event::{ClientTransactionHandle, EventQueueClass, TransactionEvent};
// Observability read handle (shared atomics, readable off the actor thread).
pub use metrics::{RetransmitRow, TransactionMetrics};
// Identifier generation seam (Via branch / To-tag), and the request-derived
// identity of a stateless answer.
pub use rng::{EntropyUnavailable, IdGen};
pub use stateless_tag::StatelessTagger;
// Whether the owner loop is turning (`TransactionLayer::owner_pulse`).
pub use pulse::LoopPulse;
// This crate's named locks and their recoveries from a panic (ADR-0051 §7).
pub use poison::{LOCKS, LOCK_RECOVERIES};

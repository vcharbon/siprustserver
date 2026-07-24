//! sip-txn — the SIP transaction layer.
//!
//! RFC 3261 §17 client/server transaction state machines: duplicate detection
//! (Via branch), retransmission + timeout timers (A/B for INVITE, E/F for
//! non-INVITE, G non-2xx-final retransmit, D/H/J holds), CANCEL→200+487, ACK
//! absorption for non-2xx, and cached-final-response retransmission. In-memory
//! only (≤ ~32 s txn lifetime).
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

pub mod event;
pub mod layer;
pub mod metrics;
pub mod rng;
pub mod timers;

// The layer actor: spawn, send API, per-call eviction, ADR-0014 self-release.
pub use layer::{TransactionConfig, TransactionLayer, TransactionLayerClosed};
// Upward events + the client-transaction handles `send_request` returns.
pub use event::{
    ClientTransactionHandle, EventQueueDropReason, TimeoutKind, TransactionEvent, TxnKind,
};
// Observability read handle (shared atomics, readable off the actor thread).
pub use metrics::TransactionMetrics;
// Identifier generation seam (Via branch / To-tag).
pub use rng::IdGen;

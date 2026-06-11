//! sip-txn — the SIP transaction layer (slice "Transaction / dispatch" of the
//! migration; port of `src/sip/TransactionLayer.ts`).
//!
//! RFC 3261 §17 client/server transaction state machines: duplicate detection
//! (Via branch), retransmission + timeout timers (A/B for INVITE, E/F for
//! non-INVITE, H/J cleanup), CANCEL→200+487, ACK absorption for non-2xx, and
//! cached-final-response retransmission. In-memory only (≤ ~32 s txn lifetime).
//!
//! Sits on a [`sip_net::UdpEndpoint`] (it parses raw datagrams itself, via a
//! [`sip_message::SipParser`]) and emits a stream of [`TransactionEvent`]s to
//! its consumer.
//!
//! ## What this crate is NOT
//!
//! The per-call FIFO dispatch — `SipRouter` + `PerCallDispatcher` (source
//! ADR-0005) — is **not** here: it is a B2BUA-only concern that depends on the
//! call layer + rule engine (both unported), whereas the transaction layer is
//! shared by the proxy and the B2BUA. The single-writer property the source's
//! dispatcher provides is, at this layer, provided structurally by the owner
//! task (one writer over the txn map). See `docs/adr/0007`.
//!
//! ## Design
//!
//! One owner task ("the actor") owns the transaction map and a single
//! [`tokio_util::time::DelayQueue`] holding every pending SIP timer — see
//! [`layer`] and docs/adr/0007 for the scalability rationale (flat memory vs.
//! task-per-timer; lock-free single-writer map vs. a shared mutex).

pub mod event;
pub mod layer;
pub mod metrics;
pub mod rng;
pub mod timers;

pub use event::{ClientTransactionHandle, EventQueueDropReason, TransactionEvent, TxnKind};
pub use layer::{TransactionConfig, TransactionLayer, TransactionLayerClosed};
pub use metrics::TransactionMetrics;
pub use rng::IdGen;

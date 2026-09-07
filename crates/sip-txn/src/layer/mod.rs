//! The transaction layer actor.
//!
//! ## Shape (see docs/adr/0007)
//!
//! **One owner task** ("the actor") that:
//!   - owns `txns` (the transaction map) — no locks, single writer;
//!   - owns one `DelayQueue` holding *every* pending SIP timer keyed by
//!     branch — flat memory at 50K calls instead of ~100K timer tasks;
//!   - `select!`s over (1) the external send API, (2) inbound packets it parses
//!     inline, (3) the next timer expiry, (4) the safety-net sweep.
//!
//! The send API (`send_request`/`send_response`/`send_raw`/
//! `cancel_txns_for_call`) funnels commands to the owner over an mpsc and
//! awaits a oneshot reply, so every mutation runs on the one writer — the
//! ADR-0005 single-writer seam, preserved without a per-call dispatcher (which
//! is a B2BUA-only concern; this layer is shared with the proxy).
//!
//! Module map: `handle` (public API + the command funnel) · `owner` (the actor
//! loop + map/wheel bookkeeping) · `client` (RFC 3261 §17.1 UAC FSM) ·
//! `server` (§17.2 UAS FSM) · `seed` (ADR-0014 seeding + re-offer) · `events`
//! (output-queue discipline) · `txn` (per-transaction state + timing policy).

mod client;
mod events;
mod handle;
mod owner;
mod seed;
mod server;
mod txn;

pub use handle::{TransactionConfig, TransactionLayer, TransactionLayerClosed};

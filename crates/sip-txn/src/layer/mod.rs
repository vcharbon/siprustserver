//! The transaction layer actor.
//!
//! ## Shape (see docs/adr/0007)
//!
//! **One owner task** ("the actor") that:
//!   - owns the transaction maps, one per role (a request matches only a
//!     server transaction, a response only a client one — RFC 3261 §17.1.3,
//!     §17.2.3) — no locks, single writer;
//!   - owns one `DelayQueue` holding *every* pending SIP timer keyed by
//!     role and branch — flat memory at 50K calls instead of ~100K timer tasks;
//!   - `select!`s over (1) the external send API, (2) inbound packets it parses
//!     inline, (3) the next timer expiry, (4) the safety-net sweep.
//!
//! The send API (`send_request`/`send_response`/`send_raw`/
//! `cancel_txns_for_call`) funnels commands to the owner over an mpsc and
//! awaits a oneshot reply, so every mutation runs on the one writer — the
//! ADR-0005 single-writer seam, preserved without a per-call dispatcher (which
//! is a B2BUA-only concern; this layer is shared with the proxy).
//!
//! Module map: `handle` (public API + the command funnel) · `run` (the actor
//! loop, one pulse-stamped turn per `select!`) · `owner` (map/wheel bookkeeping) ·
//! `call_txns` (a call's transactions, as the per-call index files them) ·
//! `via_marks` (the call attribution a sent request's Via carries) ·
//! `ingress` (an inbound datagram parsed, a parser panic contained to it) · `client` (RFC 3261 §17.1 UAC FSM) ·
//! `client_give_up` (a client transaction's deadlines and the Timeout it reports) ·
//! `accepted` (RFC 6026 §7.2: a client INVITE after its 2xx, and who ACKs it) ·
//! `orphan` (a released call's client transactions, ADR-0034) ·
//! `server` (§17.2 UAS FSM) · `server_cancel` (§9.2: a CANCEL of a server
//! INVITE) · `shrink` (a drained table's memory returned) · `lifetime` (when a transaction leaves the map:
//! its one deadline, and the sweep over it) · `bind` (the To-tag every
//! outbound response is held to) · `seed` (ADR-0014 seeding + re-offer) ·
//! `relinquish` (a call's transactions deleted for another node to serve) ·
//! `events` (output-queue discipline) · `backlog` (new-call admission against
//! the deferred backlog) · `refusals` (the node's shared refusals of new
//! INVITEs) · `txn` (per-transaction state) · `unanswered` (server
//! transactions the consumer will not answer) · `key` (a server transaction's
//! §17.2.3 identity).

mod accepted;
mod backlog;
mod bind;
mod call_txns;
mod client;
mod client_give_up;
mod events;
mod handle;
mod ingress;
mod key;
mod lifetime;
mod orphan;
mod owner;
mod parse_refusal;
mod refusals;
mod relinquish;
mod run;
mod seed;
mod server;
mod server_cancel;
mod shrink;
mod txn;
mod unanswered;
mod via_marks;

pub use backlog::{DeferredBound, InviteClass};
pub use handle::{TransactionConfig, TransactionLayer, TransactionLayerClosed};
pub use key::ServerTxnKey;
pub use refusals::{InviteRefusals, NewCallRefusal, Verdict, REFUSED_MEMO_MAX};

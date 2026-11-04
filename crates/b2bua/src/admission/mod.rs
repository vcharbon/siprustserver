//! Admission of a new INVITE (ADR-0037): the ordered ladder of rungs that may
//! refuse it, the one answer every refusal draws, and the class every rung
//! judges it in.
//!
//! | rung | site | input | normal | emergency | in dialog | Retry-After base |
//! |---|---|---|---|---|---|---|
//! | brake | arrival | inbound queue depth | at the threshold | admitted | admitted | configured |
//! | backlog | transactions | deferred events | normal ceiling | emergency ceiling | emergency ceiling | configured |
//! | event queue, on arrival | transactions | the event-queue gate's state (ADR-0050 §9) | while it refuses | admitted | admitted | configured |
//! | capacity | router ingress | calls (admitted ones not born yet included), transactions, RSS | normal ceilings | emergency ceilings | admitted | configured |
//! | shed | router ingress | live per-call queues | cap less headroom | cap | — | configured |
//! | panic-ELU | router ingress | EWMA-ELU | above the backstop | admitted | — | configured |
//! | bucket | router ingress | time to a CPS token | no token | admitted | — | time to a token, at least configured |
//! | event queue, again | router ingress | the event-queue gate, read afresh | while it refuses | admitted | — | configured |
//!
//! Each rung keeps its site. The backlog is judged by the transaction
//! layer's bound ([`deferred_bound`]); the event queue by the transaction
//! layer on arrival, right after the backlog, reading the gate's switch
//! (`sip_txn::NewCallGate`), so an INVITE a router rung would also refuse is
//! counted under it; it is checked again at router ingress, after the
//! bucket, on a fresh reading; every other rung by [`judge`] over the input
//! its site reads ([`first_refusal`] walks the router's rungs in [`LADDER`]
//! order). At router ingress the rungs run before the dispatch offer, so a
//! refusal opens no per-call queue and waits for no handler permit; the CPS token is spent
//! only when the offer queues the INVITE's turn, and a refusal spends none.
//! An admitted INVITE counts as a live call until its turn creates the call
//! or ends without one; an INVITE on the identity of a call already here
//! (live, or admitted and not born yet) is that call's copy: not judged, and
//! refused as a copy rather than born when that call never is.
//! The router's run loop is the bucket's only taker, so the token found there
//! is still there when it is taken.
//!
//! - `ladder` — [`Class`], [`class_of`], [`Rung`], [`judge`], [`LADDER`],
//!   [`first_refusal`].
//! - `render` — [`Refusals`]: the one 503 and the memo of refused INVITEs.
//! - `backlog` — the backlog rung's ceilings, handed to the transaction layer.
//!
//! Every first refusal is counted once on `b2bua_new_calls_total`
//! ([`crate::new_calls`]); a copy refused again is counted apart.

mod backlog;
mod ladder;
mod render;

pub use backlog::{ceilings, deferred_bound};
pub use ladder::{
    class_of, first_refusal, judge, Class, Refused, RouterReadings, Rung, Step, Verdict, LADDER,
};
pub use render::Refusals;

#[cfg(test)]
mod tests;

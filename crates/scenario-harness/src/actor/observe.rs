//! The **reception-observation** seam: a caller-supplied hook the executor
//! invokes with the typed message a reception goal consumed. The upstream
//! crate ships only the hook point, this context, and the mechanics; what a
//! caller DOES with an observation — classify every differing header against
//! its own reference, tally a divergence — is caller-side policy code.
//!
//! Strictly observational: the hook returns nothing and the executor reads
//! nothing back, so an installed observer cannot change a run's outcome. It
//! fires whether or not the goal carries a matcher, and whether or not that
//! matcher passed. A panic inside it unwinds through the call driver's own
//! boundary ([`run_call_with`](super::run_call_with) resumes it) — never
//! swallowed.
//!
//! # What fires, exactly
//!
//! ONE invocation per received MESSAGE a goal consumed, carrying the DUE
//! goal's cursor index. A blessed substitution
//! ([`accept_delta`](super::accept_delta)) can satisfy several goals with one
//! message: the hook still fires once, naming the due goal only, so a caller
//! tallying per-GOAL coverage joins against the
//! [`AcceptedDelta`](super::state::ReplayEntry::AcceptedDelta) observation,
//! which records the rule beside that same step.
//!
//! Only goal-driven receptions reach the hook. A provisional passed over on
//! the way to an expected final, a `100`, a response belonging to another
//! transaction, and every message the reactive core handles alone (a caller's
//! 2xx and its ACK) are consumed without a goal and are never observed.
//!
//! The observer runs SYNCHRONOUSLY inside the actor's `select!` loop: a slow or
//! blocking observer delays that actor's reactor, and on a real clock can push
//! its steps past their deadlines. Installing one also makes every response on
//! every leg retain a boxed clone for the call's lifetime.

use std::sync::Arc;

use sip_message::{SipHeader, SipRequest, SipResponse};

use super::runner::ActorState;

/// The caller-supplied observer: given one satisfied reception, look at the
/// message. Stored `Arc<dyn Fn ... + Send + Sync>` exactly like
/// [`AcceptedDeltaPolicy`](super::delta::AcceptedDeltaPolicy), so it rides the
/// `Send` plan surfaces unchanged.
pub type ReceptionObserver = Arc<dyn Fn(&ReceptionContext<'_>) + Send + Sync>;

/// What the observer sees: which actor's which goal was satisfied, and by what.
#[derive(Debug)]
pub struct ReceptionContext<'a> {
    /// The observing actor's role (the leg name).
    pub role: &'static str,
    /// The goal-cursor index of the DUE reception goal this message
    /// satisfied — the only one named, even when a substitution satisfies
    /// several with it.
    pub step: usize,
    /// The message that satisfied it, borrowed as received.
    pub received: ReceivedMessage<'a>,
}

/// The typed message a reception goal consumed — handed over WHOLE rather than
/// pre-flattened, so a caller reads whatever it needs through `sip-message`.
///
/// Below the parsed view sit two byte-exact escape hatches on the inner
/// message: `image()` is the datagram exactly as it arrived, and
/// `sip_message::parser::custom::headers::scan_header_name_forms(image, limits)`
/// recovers the wire header names — compact forms included — aligned 1:1 with
/// [`headers`](Self::headers). A check that must be byte-exact on name form or
/// on line folding reads `image()`; [`headers`](Self::headers) cannot answer
/// those (see its own contract).
#[derive(Debug, Clone, Copy)]
pub enum ReceivedMessage<'a> {
    /// An inbound request an `ExpectRequest` consumed.
    Request(&'a SipRequest),
    /// An inbound response a response-consuming reception goal consumed.
    Response(&'a SipResponse),
}

impl ReceivedMessage<'_> {
    /// The message's full header list as the parser produced it, in wire order.
    ///
    /// GUARANTEED, so a comparison may rely on it: wire order; every repeat a
    /// DISTINCT entry, never deduped or merged; a comma-folded value kept
    /// whole, never split into its list items; and the wire casing of any name
    /// that is not a compact form (`X-FoLdEd` survives verbatim).
    ///
    /// NORMALIZED by the parser, so a comparison must not assume it: a
    /// single-character compact name is expanded to its canonical long form
    /// (RFC 3261 §7.3.3 — `v` → `Via`, `f` → `From`, …, in any case); a value
    /// folded across lines is rejoined with one inserted space, its
    /// continuation whitespace kept; and non-UTF-8 value bytes become U+FFFD.
    ///
    /// Byte-exact questions about name form or folding go to `image()` instead
    /// (see [`ReceivedMessage`]).
    pub fn headers(&self) -> &[SipHeader] {
        match self {
            ReceivedMessage::Request(r) => r.headers(),
            ReceivedMessage::Response(r) => r.headers(),
        }
    }

    /// The message body, verbatim.
    pub fn body(&self) -> &[u8] {
        match self {
            ReceivedMessage::Request(r) => r.body(),
            ReceivedMessage::Response(r) => r.body(),
        }
    }
}

/// Hand one satisfied reception to the plan's observer. A no-op when none is
/// installed — the hook-absent path builds no context and borrows nothing.
pub(super) fn observe_reception(st: &ActorState<'_>, received: ReceivedMessage<'_>) {
    if let Some(observer) = &st.reception_observer {
        observer(&ReceptionContext { role: st.role, step: st.goals.position(), received });
    }
}

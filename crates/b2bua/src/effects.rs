//! The typed side-effect vocabulary a handler returns — port of
//! `SipRouter.ts` `HandlerEffects` / `HandlerResult`. The router's
//! `process_result` interpreter runs the five categories in a fixed order with
//! different safety wraps (see ADR-0003 in the source / ADR-0010 here).

use call::{Call, RetainedEmission, TimerEntry};
use sip_message::{SipRequest, SipResponse};
use sip_txn::TxnKind;

use crate::event::CallEvent;

/// How an outbound message reaches the wire.
#[derive(Debug, Clone)]
pub enum OutboundTxnMode {
    /// Allocate a new client transaction (INVITE / BYE / OPTIONS / INFO / …)
    /// — retransmits + Timer B/F are managed by the transaction layer.
    NewClient(TxnKind),
    /// Send a UAS response through its server transaction.
    ServerResponse,
    /// No client transaction of its own (ACK-for-2xx, CANCEL). An ACK bypasses
    /// the transaction layer outright; a CANCEL is routed through its INVITE
    /// client txn, which owns WHEN it goes on the wire (RFC 3261 §9.1: held
    /// until the branch's first provisional, dropped if the txn dies first).
    /// Requests only: a response never bypasses its server transaction as a
    /// `Response` — the one raw path for response bytes is a retained
    /// [`OutboundBody::Datagram`] (ADR-0032 X3).
    Raw,
}

/// The outbound payload — a request, a response, or a retained datagram.
#[derive(Debug, Clone)]
pub enum OutboundBody {
    Request(SipRequest),
    Response(SipResponse),
    /// A retained emission repeated as the bytes it left as (ADR-0032 X3):
    /// they reach the socket with no parse and no serialize, so a repeat
    /// cannot differ from the message it repeats. Always raw, whatever the
    /// mode says — the transaction that emitted the original is `Completed`
    /// or never existed. The emission's own label says what is repeated and
    /// what paced it; the datagram is never read for either.
    Datagram(RetainedEmission),
}

/// Whose message an outbound emission carries, as the message ring records
/// it. A retained datagram's repeat carries the value of the message it
/// repeats and is never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// A peer leg's message forwarded (the RFC 3261 §16 half of a B2BUA), or
    /// sent in reaction to one — a masked or promoted provisional, a failure
    /// restated from the callee's final.
    Relayed,
    /// This stack's own: a UAS/UAC-authored final, an ACK or PRACK it owes, a
    /// teardown, a decision's reject.
    Authored,
    /// A liveness probe this stack originates — the in-dialog OPTIONS
    /// keepalive. Its own, and not dialog history: the ring records neither
    /// it nor its answer.
    Probe,
}

/// One SIP message to emit.
#[derive(Debug, Clone)]
pub struct OutboundSipEffect {
    pub body: OutboundBody,
    pub mode: OutboundTxnMode,
    pub destination: (String, u16),
    pub label: String,
    pub leg_id: Option<String>,
    pub provenance: Provenance,
}

/// Critical state effects — run first, under an uninterruptible wrap; state is
/// already persisted before these execute.
#[derive(Debug, Clone)]
pub enum CriticalStateEffect {
    ScheduleTimer(TimerEntry),
    CancelTimer {
        id: String,
    },
    CancelAllTimers,
    /// Flush the call to the store (replication path).
    Flush,
    /// Remove the call from memory + store, cancel its txns, poison its queue.
    RemoveCall,
}

/// Soft-bounded effects — limiter decrements with a short timeout (never block).
#[derive(Debug, Clone)]
pub enum SoftBoundedEffect {
    DecrementLimiter { limiter_id: String, window: i64 },
}

/// Buffered observability effects — drop-on-overload is acceptable.
#[derive(Debug, Clone)]
pub enum BufferedObservabilityEffect {
    WriteCdr,
    /// A final of `status` toward the a-leg's initial INVITE was refused: that
    /// transaction already carries `carried` (RFC 3261 §17.2.1). The router
    /// counts it as `second_final_refused`.
    SecondFinalRefused {
        status: u16,
        carried: u16,
    },
    /// An asynchronous trigger (`event`: timer / timeout / internal-event)
    /// landed on a call already going away and `rule` — the highest-registered
    /// non-teardown rule it matched — was kept from running. The router counts
    /// it as `going_away_absorbed`.
    GoingAwayAbsorbed {
        event: &'static str,
        rule: &'static str,
    },
    /// A call reached `Terminated` carrying no termination record: a path to
    /// terminal states no cause. The router counts it as
    /// `termination_unrecorded`.
    TerminationUnrecorded,
}

/// Fire-and-forget effects — detached work / re-entrant events.
#[derive(Debug, Clone)]
pub enum FireAndForgetEffect {
    ReferAsyncHttp {
        call_ref: String,
        request: serde_json::Value,
    },
    /// The generic service-authorable async-HTTP callback (generalizes
    /// `ReferAsyncHttp`/`FailureAsyncHttp`). A TYPED variant: the request `body`
    /// is a raw `Vec<u8>` (NOT a `serde_json::Value`), so an arbitrary BINARY
    /// payload rides to the wire and the response bytes round-trip back verbatim.
    /// The router fires it over the host-injected `AdaptationHttpPort` and
    /// re-enters via a `service-http-result` internal event whose entity bytes
    /// ride `CallEvent::InternalEvent::body`.
    ServiceHttpRequest {
        call_ref: String,
        correlation_id: String,
        endpoint: String,
        method: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        content_type: Option<String>,
        timeout_ms: Option<u64>,
    },
    /// Kick the async `/call/failure` decision (b-leg failover). Carries the
    /// request JSON the seed rule built; the router calls `decision.call_failure`
    /// then re-enters via a `call-failure-result` internal event.
    FailureAsyncHttp {
        call_ref: String,
        request: serde_json::Value,
    },
    /// Kick the async `call_release` consult for a subscribed internal release
    /// event. Carries the event-scoped request JSON the
    /// `max-duration` rule built; the router attaches the snapshot, calls
    /// `decision.call_release` (deadline-bounded), then re-enters via a
    /// `call-release-result` internal event (`release` | `reroute`).
    ReleaseAsyncHttp {
        call_ref: String,
        request: serde_json::Value,
    },
    /// Re-enter the handler chain with an internally-generated event.
    Reenter(Box<CallEvent>),
}

/// The five categories of effect a handler emits.
#[derive(Debug, Clone, Default)]
pub struct HandlerEffects {
    pub critical: Vec<CriticalStateEffect>,
    pub outbound: Vec<OutboundSipEffect>,
    pub soft: Vec<SoftBoundedEffect>,
    pub buffered: Vec<BufferedObservabilityEffect>,
    pub fire_and_forget: Vec<FireAndForgetEffect>,
}

impl HandlerEffects {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append another effect set (used to merge composed-rule / framework
    /// effects into the rule's own).
    pub fn extend(&mut self, other: HandlerEffects) {
        self.critical.extend(other.critical);
        self.outbound.extend(other.outbound);
        self.soft.extend(other.soft);
        self.buffered.extend(other.buffered);
        self.fire_and_forget.extend(other.fire_and_forget);
    }
}

/// What a handler returns: the (immutably) updated call + its effects.
#[derive(Debug, Clone)]
pub struct HandlerResult {
    pub call: Call,
    pub effects: HandlerEffects,
}

impl HandlerResult {
    pub fn new(call: Call) -> Self {
        Self { call, effects: HandlerEffects::new() }
    }
}

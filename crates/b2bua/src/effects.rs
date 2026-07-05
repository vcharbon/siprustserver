//! The typed side-effect vocabulary a handler returns — port of
//! `SipRouter.ts` `HandlerEffects` / `HandlerResult`. The router's
//! `process_result` interpreter runs the five categories in a fixed order with
//! different safety wraps (see ADR-0003 in the source / ADR-0010 here).

use call::{Call, TimerEntry};
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
    /// Send raw, bypassing transaction management (ACK-for-2xx, CANCEL).
    Raw,
}

/// The outbound payload — a request or a response.
#[derive(Debug, Clone)]
pub enum OutboundBody {
    Request(SipRequest),
    Response(SipResponse),
}

/// One SIP message to emit.
#[derive(Debug, Clone)]
pub struct OutboundSipEffect {
    pub body: OutboundBody,
    pub mode: OutboundTxnMode,
    pub destination: (String, u16),
    pub label: String,
    pub leg_id: Option<String>,
}

/// Critical state effects — run first, under an uninterruptible wrap; state is
/// already persisted before these execute.
#[derive(Debug, Clone)]
pub enum CriticalStateEffect {
    ScheduleTimer(TimerEntry),
    CancelTimer { id: String },
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
        Self {
            call,
            effects: HandlerEffects::new(),
        }
    }
}

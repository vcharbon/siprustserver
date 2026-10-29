//! The typed side-effect vocabulary a handler returns — port of
//! `SipRouter.ts` `HandlerEffects` / `HandlerResult`. The router's
//! `process_result` interpreter runs the five categories in a fixed order with
//! different safety wraps (see ADR-0003 in the source / ADR-0010 here).

use call::TxnKind;
use call::{Call, RetainedEmission, TimerEntry, TimerKey};
use sip_message::{SipRequest, SipResponse};

use b2bua_sdk::event::CallEvent;

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
        key: TimerKey,
    },
    CancelAllTimers,
    /// Remove the call from memory + store, cancel its txns, poison its queue.
    RemoveCall,
    /// The INVITE server transaction a request with this top Via opened was
    /// answered by another node (ADR-0055 §4): it is relinquished here,
    /// nothing sent and nothing owed, so no final follows that node's
    /// (RFC 3261 §13.3.1.4, §17.2.1).
    RelinquishAnsweredInvite {
        via: sip_message::header::Via,
    },
}

/// Soft-bounded effects — limiter work handed off the call's turn (never
/// awaited by the call).
#[derive(Debug, Clone)]
pub enum SoftBoundedEffect {
    /// Release the limiter set held under `key`: the key joins the worker's
    /// release queue, and the call's turn goes on at once. Idempotent on the
    /// server. The terminal settle emits it once for a call that owes its
    /// release, under the call's own key; a route fold carrying another
    /// call's key releases that key.
    ReleaseLimiter { key: String },
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
    /// A provisional of `status` toward the a-leg's initial INVITE was
    /// refused: that transaction already sent its final `carried` and emits
    /// no further provisional (RFC 3261 §13.3.1.1 / §17.2.1). The router
    /// counts it as `provisional_after_final_refused`.
    ProvisionalAfterFinalRefused {
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
    /// Text stated from outside the stack — a header line, a reason phrase,
    /// a part line — did not fit its place on the wire and was left out of
    /// the message, `detail` naming it. The router reports it as the model
    /// defect [`crate::turn_notes::STATED_TEXT_FITS`].
    StatedTextSkipped {
        detail: String,
    },
}

impl BufferedObservabilityEffect {
    /// One [`StatedTextSkipped`](Self::StatedTextSkipped) per refusal of
    /// `skipped`, each named by `what` was being written.
    pub fn skipped(what: &str, skipped: Vec<sip_message::write_check::WriteRefusal>) -> Vec<Self> {
        skipped
            .into_iter()
            .map(|refusal| Self::StatedTextSkipped { detail: format!("{what}: {refusal}") })
            .collect()
    }
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
    /// then re-enters via a `call-failure-result` internal event. The admits
    /// of the failover chain are numbered from `limiter.first_change` on, in
    /// the block the dispatching turn reserved; `limiter.may_hold` states
    /// whether the limiter may hold a set for the call before the consult.
    /// `deadline` names the answer deadline the sending turn armed
    /// (ADR-0039), which the fold states; `None` until armed, and for a
    /// consult with no deadline.
    FailureAsyncHttp {
        call_ref: String,
        request: serde_json::Value,
        limiter: call::ConsultAdmits,
        deadline: Option<u64>,
    },
    /// Kick the async `call_release` consult for a subscribed internal release
    /// event. Carries the event-scoped request JSON the
    /// `max-duration` rule built; the router attaches the snapshot, calls
    /// `decision.call_release` (deadline-bounded), then re-enters via a
    /// `call-release-result` internal event (`release` | `reroute`).
    ReleaseAsyncHttp {
        call_ref: String,
        request: serde_json::Value,
        /// The change number a reroute's admit carries, and whether the
        /// limiter may hold a set for the call before the consult.
        limiter: call::ConsultAdmits,
        /// The change number the consult's answer deadline is armed under
        /// (`answer_deadline::arm`); `None` for an unbounded consult.
        deadline: Option<u64>,
    },
    /// Replace the call's admission set on the call limiter with `entries`
    /// under `change`, carrying the call's `held` set of the sending turn (a
    /// service's `ReplaceAdmissionSet`), then re-enter via a
    /// `limiter-admit-result` internal event echoing `correlation_id`.
    LimiterAdmit {
        call_ref: String,
        correlation_id: String,
        key: String,
        change: u64,
        held: call::LimiterHeld,
        entries: Vec<call::LimiterEntry>,
    },
    /// Re-enter the handler chain with an internally-generated event.
    Reenter(Box<CallEvent>),
}

/// A dialog-level turn that changes no replicated fact — a retransmission, or
/// an ACK owed nowhere — the closed list of turns the store replaces without a
/// version bump and the router persists without a flush (ADR-0014, "a counter
/// counts writes that change the call, not progress"). A further member is an
/// ADR line first.
///
/// A transaction-level retransmission never reaches the call model; every
/// other turn is a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuietTurn {
    /// A rung of this node's own ladder that left a copy and armed the next
    /// rung: the un-ACKed 2xx ladder (RFC 3261 §13.3.1.4) or the un-PRACKed
    /// reliable-provisional ladder (RFC 3262 §3). The turn's body delta is the
    /// rung index and the rung timer's next due instant; a rung that ceases
    /// the ladder or fires spent cancels a ledger entry and is a write.
    OwnRung,
    /// The re-ACK of a repeated inbound 2xx (RFC 3261 §13.2.2.4): the retained
    /// ACK's bytes leave again and the body records nothing but the in-dialog
    /// message count.
    ReAck,
    /// An inbound ACK the rules absorb: it discharges nothing this node awaits
    /// and no ACK is owed toward the peer for it (§13.2.2.4), so nothing
    /// leaves and the body records nothing but the in-dialog message count.
    AbsorbedAck,
}

impl QuietTurn {
    /// Every kind, in declaration order.
    pub const ALL: [QuietTurn; 3] = [QuietTurn::OwnRung, QuietTurn::ReAck, QuietTurn::AbsorbedAck];

    /// The `kind` label of `b2bua_repl_quiet_turns_total`.
    pub const fn kind(self) -> &'static str {
        match self {
            QuietTurn::OwnRung => "own-rung",
            QuietTurn::ReAck => "re-ack",
            QuietTurn::AbsorbedAck => "absorbed-ack",
        }
    }
}

/// The five categories of effect a handler emits, and the turn's class.
#[derive(Debug, Clone, Default)]
pub struct HandlerEffects {
    pub critical: Vec<CriticalStateEffect>,
    pub outbound: Vec<OutboundSipEffect>,
    pub soft: Vec<SoftBoundedEffect>,
    pub buffered: Vec<BufferedObservabilityEffect>,
    pub fire_and_forget: Vec<FireAndForgetEffect>,
    /// The [`QuietTurn`] candidate, set by exactly three authors — the
    /// framework's rung turn, the bodyless re-ACK and the framework's ACK turn
    /// the rules absorbed; the router persists the turn quietly only when its
    /// whole effect set is that repeat, or nothing for an absorbed ACK.
    pub quiet: Option<QuietTurn>,
    /// The attempts the turn opened, each with the routing answer it dials:
    /// what the turn's `AttemptStarted` deliveries carry
    /// (`rules::lifecycle`). Turn-local, never replicated.
    pub opened_attempts: Vec<(call::AttemptId, b2bua_sdk::lifecycle::RouteAnswer)>,
    /// The turn was taken on the answer to a request the call sent off its
    /// turn, or on its deadline's expiry (ADR-0039). Turn-local, never
    /// replicated.
    pub took_answer: bool,
    /// The turn sent again requests another node had sent: off the call's
    /// turn, or its routing decision. Turn-local, never replicated.
    pub resent: bool,
}

impl HandlerEffects {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append another effect set (used to merge composed-rule / framework
    /// effects into the rule's own). A quiet candidate carries over; the
    /// merged effect set is what the router classifies.
    pub fn extend(&mut self, other: HandlerEffects) {
        self.quiet = self.quiet.or(other.quiet);
        self.took_answer |= other.took_answer;
        self.resent |= other.resent;
        self.critical.extend(other.critical);
        self.outbound.extend(other.outbound);
        self.soft.extend(other.soft);
        self.buffered.extend(other.buffered);
        self.fire_and_forget.extend(other.fire_and_forget);
        self.opened_attempts.extend(other.opened_attempts);
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

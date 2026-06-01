//! The rule-engine core types — port of `RuleDefinition.ts` (declarative
//! `Match`, `RuleContext`, the `RuleAction` vocabulary) reduced to a concrete
//! (non-type-narrowed) shape sufficient for the basic B2BUA. Service-layer
//! ext narrowing and rule composition are deferred with their consumers.

use call::{
    Call, CallModelState, CdrEventType, Dialog, Direction, Leg, LegDisposition, LegState, TimerType,
};
use sip_message::{SipRequest, SipResponse};

use crate::config::B2buaConfig;
use crate::event::CallEvent;

pub const CORE_LAYER: u8 = 0;
#[allow(dead_code)]
pub const SERVICE_LAYER: u8 = 1;

/// Status matcher for a response rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusMatch {
    Any,
    /// Exact status code.
    Code(u16),
    /// Status class (1 = 1xx, 2 = 2xx, …).
    Class(u8),
}

impl StatusMatch {
    fn accepts(self, status: u16) -> bool {
        match self {
            StatusMatch::Any => true,
            StatusMatch::Code(c) => c == status,
            StatusMatch::Class(c) => (status / 100) as u8 == c,
        }
    }
}

/// Which event family a rule intercepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Request,
    Response,
    Timer,
    Timeout,
    Cancelled,
    InternalEvent,
}

/// A declarative match descriptor — every omitted column matches any value.
#[derive(Clone)]
pub struct Match {
    pub kind: MatchKind,
    /// Request method / response cseq-method / timeout method (case-insensitive).
    pub methods: Option<Vec<&'static str>>,
    pub status: StatusMatch,
    pub timer_types: Option<Vec<TimerType>>,
    pub call_state: Option<Vec<CallModelState>>,
    pub leg_state: Option<Vec<LegState>>,
    pub leg_disposition: Option<Vec<LegDisposition>>,
    pub direction: Option<Direction>,
    pub topic: Option<&'static str>,
    pub outcome: Option<&'static str>,
    /// A corner-case predicate (pure, sync) over the resolved context.
    pub filter: Option<fn(&RuleContext) -> bool>,
}

impl Match {
    fn base(kind: MatchKind) -> Self {
        Self {
            kind,
            methods: None,
            status: StatusMatch::Any,
            timer_types: None,
            call_state: None,
            leg_state: None,
            leg_disposition: None,
            direction: None,
            topic: None,
            outcome: None,
            filter: None,
        }
    }
    pub fn request() -> Self {
        Self::base(MatchKind::Request)
    }
    pub fn response() -> Self {
        Self::base(MatchKind::Response)
    }
    pub fn timer() -> Self {
        Self::base(MatchKind::Timer)
    }
    pub fn timeout() -> Self {
        Self::base(MatchKind::Timeout)
    }
    pub fn cancelled() -> Self {
        Self::base(MatchKind::Cancelled)
    }
    pub fn internal_event() -> Self {
        Self::base(MatchKind::InternalEvent)
    }
    pub fn topic(mut self, t: &'static str) -> Self {
        self.topic = Some(t);
        self
    }
    pub fn outcome(mut self, o: &'static str) -> Self {
        self.outcome = Some(o);
        self
    }
    pub fn method(mut self, m: &'static str) -> Self {
        self.methods.get_or_insert_with(Vec::new).push(m);
        self
    }
    pub fn methods(mut self, ms: &[&'static str]) -> Self {
        self.methods = Some(ms.to_vec());
        self
    }
    pub fn status_code(mut self, c: u16) -> Self {
        self.status = StatusMatch::Code(c);
        self
    }
    pub fn status_class(mut self, c: u8) -> Self {
        self.status = StatusMatch::Class(c);
        self
    }
    pub fn timer_type(mut self, t: TimerType) -> Self {
        self.timer_types.get_or_insert_with(Vec::new).push(t);
        self
    }
    pub fn call_state(mut self, s: CallModelState) -> Self {
        self.call_state.get_or_insert_with(Vec::new).push(s);
        self
    }
    pub fn leg_states(mut self, s: &[LegState]) -> Self {
        self.leg_state = Some(s.to_vec());
        self
    }
    pub fn leg_disposition(mut self, d: LegDisposition) -> Self {
        self.leg_disposition.get_or_insert_with(Vec::new).push(d);
        self
    }
    pub fn direction(mut self, d: Direction) -> Self {
        self.direction = Some(d);
        self
    }
    pub fn filter(mut self, f: fn(&RuleContext) -> bool) -> Self {
        self.filter = Some(f);
        self
    }

    /// Does `ctx`'s event match this descriptor (columns only; the `filter` is
    /// applied by the matcher after column acceptance)?
    pub fn accepts_columns(&self, ctx: &RuleContext) -> bool {
        // Event-kind gate.
        let kind_ok = matches!(
            (self.kind, ctx.event),
            (MatchKind::Request, CallEvent::Sip { .. })
                | (MatchKind::Response, CallEvent::Sip { .. })
                | (MatchKind::Timer, CallEvent::Timer { .. })
                | (MatchKind::Timeout, CallEvent::Timeout { .. })
                | (MatchKind::Cancelled, CallEvent::Cancelled { .. })
                | (MatchKind::InternalEvent, CallEvent::InternalEvent { .. })
        );
        if !kind_ok {
            return false;
        }
        // For Sip events, request-rules only match requests and response-rules
        // only responses.
        match (self.kind, ctx.request(), ctx.response()) {
            (MatchKind::Request, Some(_), _) => {}
            (MatchKind::Request, None, _) => return false,
            (MatchKind::Response, _, Some(_)) => {}
            (MatchKind::Response, _, None) => return false,
            _ => {}
        }

        if let Some(methods) = &self.methods {
            let m = match self.kind {
                MatchKind::Request => ctx.request().map(|r| r.method.clone()),
                MatchKind::Response => ctx.response().map(|r| r.cseq.method.clone()),
                MatchKind::Timeout => ctx.timeout_method().map(str::to_string),
                _ => None,
            };
            match m {
                Some(m) if methods.iter().any(|x| x.eq_ignore_ascii_case(&m)) => {}
                _ => return false,
            }
        }

        if self.kind == MatchKind::Response {
            if let Some(r) = ctx.response() {
                if !self.status.accepts(r.status) {
                    return false;
                }
            }
        }

        if let Some(tts) = &self.timer_types {
            match ctx.timer_type() {
                Some(t) if tts.contains(&t) => {}
                _ => return false,
            }
        }

        if let Some(states) = &self.call_state {
            if !states.contains(&ctx.call.state) {
                return false;
            }
        }
        if let Some(states) = &self.leg_state {
            match ctx.source_leg() {
                Some(leg) if states.contains(&leg.state) => {}
                _ => return false,
            }
        }
        if let Some(disps) = &self.leg_disposition {
            match ctx.source_leg() {
                Some(leg) if disps.contains(&leg.disposition) => {}
                _ => return false,
            }
        }
        if let Some(dir) = self.direction {
            if ctx.direction != dir {
                return false;
            }
        }
        if let Some(topic) = self.topic {
            if !matches!(ctx.event, CallEvent::InternalEvent { topic: t, .. } if t == topic) {
                return false;
            }
        }
        if let Some(outcome) = self.outcome {
            if !matches!(ctx.event, CallEvent::InternalEvent { outcome: o, .. } if o == outcome) {
                return false;
            }
        }
        true
    }
}

/// A rule: a named, layer-ranked event handler that returns actions or declines.
#[derive(Clone)]
pub struct RuleDefinition {
    pub id: &'static str,
    pub layer: u8,
    pub overrides: &'static [&'static str],
    pub matcher: Match,
    pub handle: fn(&RuleContext) -> Option<RuleHandleResult>,
}

/// What a rule emits when it handles an event.
#[derive(Debug, Clone, Default)]
pub struct RuleHandleResult {
    pub actions: Vec<RuleAction>,
}

impl RuleHandleResult {
    pub fn new(actions: Vec<RuleAction>) -> Self {
        Self { actions }
    }
}

/// Transform applied to a relayed message.
#[derive(Debug, Clone, Default)]
pub struct MessageTransform {
    pub status: Option<u16>,
    pub reason: Option<String>,
    /// Drop the body (+ Content-Type) — bare-180 downgrade (`relayFirst18x`).
    pub drop_body: bool,
    /// Passthrough headers to suppress on the relayed message (case-insensitive),
    /// e.g. `Require`/`RSeq` when downgrading a reliable 18x to a bare 180.
    pub remove_headers: Vec<&'static str>,
    /// Headers to stamp on the relayed message with replace semantics (the
    /// synthetic-200 / resync-reINVITE Allow + Supported, `promote18xPemTo200`).
    pub add_headers: Vec<(&'static str, String)>,
}

/// The action vocabulary. The basic-B2BUA subset is exercised now; the trailing
/// cluster (provisional/prack/notify/reinvite/sdp/policy/refer) is defined for
/// the deferred 18x/transfer service rules and is unused until they land.
#[derive(Debug, Clone)]
pub enum RuleAction {
    RelayToPeer { transform: MessageTransform },
    RelayToLeg { leg_id: String, transform: MessageTransform },
    Respond {
        status: u16,
        reason: String,
        body: Vec<u8>,
        content_type: Option<String>,
    },
    AckLeg { leg_id: String },
    ConfirmDialog { leg_id: String },
    UpdateLegState {
        leg_id: String,
        state: LegState,
        disposition: Option<LegDisposition>,
    },
    AddTagMapping {
        a_tag: String,
        b_leg_id: String,
        b_tag: String,
    },
    Merge { leg_a: String, leg_b: String },
    Split { leg_id: String },
    CreateLeg {
        destination: (String, u16),
        new_ruri: Option<String>,
        no_answer_timeout_sec: Option<i64>,
        callback_context: Option<String>,
        /// Body override for the C INVITE (REFER transfer held SDP). `None`
        /// keeps A's INVITE body; `Some(bytes)` replaces it (empty = drop).
        body_override: Option<Vec<u8>>,
        /// Header overrides applied to the C INVITE (`update_headers` from the
        /// /call/refer allow). `(name, Some(value))` sets, `(name, None)` removes.
        header_updates: Vec<(String, Option<String>)>,
    },
    DestroyLeg { leg_id: String },
    CancelLeg { leg_id: String },
    ScheduleTimer {
        timer_type: TimerType,
        delay_sec: i64,
        leg_id: Option<String>,
    },
    CancelTimer { id: String },
    CancelAllTimers,
    TerminateCall,
    BeginTermination { reason: Option<String> },
    TerminateLeg {
        leg_id: String,
        bye_disposition: Option<call::ByeDisposition>,
    },
    AddCdrEvent {
        event_type: CdrEventType,
        leg_id: String,
        status_code: Option<i64>,
        reason: Option<String>,
    },
    DeactivateRule { rule_id: String },
    SendRequestToLeg { leg_id: String, method: String },
    // ── relayFirst18xTo180 (SERVICE_LAYER) ──────────────────────────────────
    /// Originate a PRACK toward `leg_id`'s early dialog (selected by `b_tag`),
    /// acknowledging the reliable 1xx with RAck `<rseq> <invite_cseq> INVITE`
    /// (RFC 3262 §7.2). The B2BUA PRACKs the b-leg itself because alice never
    /// saw the reliable provisional (it was downgraded to a bare 180).
    SendPrackToLeg {
        leg_id: String,
        rseq: i64,
        invite_cseq: i64,
        b_tag: String,
    },
    /// Cache an SDP body on a b-leg dialog (selected by `b_tag`) for later
    /// substitution into the 200 OK toward alice (`fake-prack`).
    CacheSdpOnLegDialog {
        leg_id: String,
        b_tag: String,
        body: Vec<u8>,
    },
    /// Stage `body` into `call.policy_update_body` so the response relay path
    /// substitutes it into the next relayed body (`fake-prack` 200-OK SDP).
    SetPolicyUpdateBody { body: Vec<u8> },
    /// First-18x bare-180 downgrade (`relayFirst18xTo180`): mint an a-facing
    /// To-tag (the executor owns the IdGen), seed the tag map for this b-leg
    /// dialog, record it as `stored_a_tag` + `first_relayed`, and relay the
    /// current 1xx to the caller as a bare 180 (no body / Require / RSeq). The
    /// minted tag is the single source the relay path resolves via the tag map.
    RelayFirstBare180 { leg_id: String, b_tag: String },
    // ── promote18xPemTo200 (SERVICE_LAYER) ──────────────────────────────────
    /// Originate a re-INVITE on `leg_id` (here always the a-leg) carrying `body`
    /// as the new offer plus `add_headers` (Allow/Supported), CSeq =
    /// dialog.localCSeq + 1. Used to resync Alice when bob's final SDP differs
    /// from the early-media SDP that was promoted into the synthetic 200 OK.
    SendReinvite {
        leg_id: String,
        body: Vec<u8>,
        add_headers: Vec<(&'static str, String)>,
    },
    /// Overwrite the per-call PEM runtime slice (`None` → pre-promotion state).
    SetPromotePem { state: Option<call::PromotePemState> },
    // ── referTransfer (SERVICE_LAYER) ───────────────────────────────────────
    /// Send a NOTIFY toward `leg_id`'s confirmed dialog carrying the REFER
    /// subscription state (`Event` + `Subscription-State` + sipfrag body). The
    /// B2BUA is the UAS of the referrer leg, so the NOTIFY rides that dialog.
    /// Port of `executeSendNotify` (ActionExecutor.ts:2157).
    SendNotify {
        leg_id: String,
        event: String,
        subscription_state: String,
        content_type: Option<String>,
        body: Vec<u8>,
    },
    /// Kick the async /call/refer authorization: push a `ReferAsyncHttp`
    /// fire-and-forget effect carrying the request JSON. The router interpreter
    /// calls `decision.call_refer` then re-enters via a `refer-http-result`
    /// internal event.
    ReferAsyncHttp { request: serde_json::Value },
    /// Overwrite the per-call REFER transfer runtime slice (`None` clears it —
    /// the terminal path; mirrors `SetPromotePem`).
    SetTransfer { state: Option<call::TransferState> },
    // ── b-leg failover (/call/failure) ───────────────────────────────────────
    /// Kick the async `/call/failure` decision: push a `FailureAsyncHttp`
    /// fire-and-forget effect carrying the request JSON. The router interpreter
    /// calls `decision.call_failure` then re-enters via a `call-failure-result`
    /// internal event.
    FailureAsyncHttp { request: serde_json::Value },
    /// Synthesize a final failure response on the a-leg INVITE server txn
    /// (the terminate-after-`/call/failure` path — relay the b-leg failure to A
    /// once the backend declines to fail over). Reuses the a-dialog tag.
    RelayFailureToALeg { status: u16, reason: String },
}

/// The resolved context a rule sees. Built by the executor/router from the
/// event + call (the source's match-narrowed `RuleContext`, flattened).
pub struct RuleContext<'a> {
    pub call: &'a Call,
    pub call_ref: &'a str,
    pub event: &'a CallEvent,
    pub source_leg_id: &'a str,
    pub direction: Direction,
    pub now_ms: i64,
    pub config: &'a B2buaConfig,
}

impl<'a> RuleContext<'a> {
    pub fn request(&self) -> Option<&SipRequest> {
        match self.event {
            CallEvent::Sip { message, .. } => match message.as_ref() {
                sip_message::SipMessage::Request(r) => Some(r),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn response(&self) -> Option<&SipResponse> {
        match self.event {
            CallEvent::Sip { message, .. } => match message.as_ref() {
                sip_message::SipMessage::Response(r) => Some(r),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn timer_type(&self) -> Option<TimerType> {
        match self.event {
            CallEvent::Timer { timer_type, .. } => Some(*timer_type),
            _ => None,
        }
    }
    pub fn timeout_method(&self) -> Option<&str> {
        match self.event {
            CallEvent::Timeout { method, .. } => method.as_deref(),
            _ => None,
        }
    }
    /// The leg the event arrived on.
    pub fn source_leg(&self) -> Option<&Leg> {
        if self.source_leg_id == self.call.a_leg.leg_id {
            Some(&self.call.a_leg)
        } else {
            self.call.b_legs.iter().find(|l| l.leg_id == self.source_leg_id)
        }
    }
    /// The dialog the event is on (confirmed dialog, else the first).
    pub fn source_dialog(&self) -> Option<&Dialog> {
        let leg = self.source_leg()?;
        call::helpers::confirmed_dialog(leg).or_else(|| leg.dialogs.first())
    }
}

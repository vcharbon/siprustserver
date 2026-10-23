//! The rule-engine core types — port of `RuleDefinition.ts` (declarative
//! `Match`, `RuleContext`, the `RuleAction` vocabulary) reduced to a concrete
//! (non-type-narrowed) shape sufficient for the basic B2BUA. Service-layer
//! ext narrowing and rule composition are deferred with their consumers.

use call::features::FeatureActivations;
use call::{
    CallModelState, CdrEventType, Dialog, Direction, ExtMap, Leg, LegDisposition, LegKind,
    LegState, MachineId, Obligation, StateLabel, TerminationCause, TimerKey, TimerType,
};
use sip_message::draft::Entry;
use sip_message::header::HeaderName;
use sip_message::{Method, SipRequest, SipResponse};

pub use crate::body::{Body, BodyAuthor};
use crate::config::B2buaConfig;
pub use crate::effect::{Effect, EffectKind};
use crate::event::CallEvent;
pub use crate::glare::GlareRefusal;
pub use crate::leg_end::LegEnd;
pub use crate::lifecycle::{AttemptOpening, LifecycleEvent, RouteAnswer};
use crate::relayed_final::RelayedFinal;
pub use crate::rule_call::RuleCall;
pub use crate::timer_delay::TimerDelay;

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
            StatusMatch::Class(c) => status / 100 == u16::from(c),
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
    /// A lifecycle event the engine delivers to the active services after a
    /// turn's claim ([`crate::lifecycle`]); only a machine-bound rule
    /// receives one.
    Lifecycle,
}

/// A declarative match descriptor — every omitted column matches any value.
#[derive(Clone)]
pub struct Match {
    pub kind: MatchKind,
    /// Request method / response cseq-method / timeout method (case-insensitive).
    pub methods: Option<Vec<&'static str>>,
    pub status: StatusMatch,
    /// Exact timer-type matching — for a [`TimerType::Service`] entry this is
    /// exact `(service_id, key)` equality (the leg, if any, is not part of the
    /// type). Use [`Self::service_timers`] for a per-service any-key wildcard.
    pub timer_types: Option<Vec<TimerType>>,
    /// Per-service timer wildcard: accept any [`TimerType::Service`] firing
    /// whose `service_id` matches, regardless of `key`. A service can therefore
    /// funnel all its watchdogs through one rule and branch on
    /// [`RuleContext::service_timer_key`]. Never matches a core timer, and a
    /// rule must name a `MachineId` to receive service fires at all — so no
    /// rule catches another service's (or core's) timers by accident.
    pub timer_service: Option<MachineId>,
    pub call_state: Option<Vec<CallModelState>>,
    pub leg_state: Option<Vec<LegState>>,
    pub leg_disposition: Option<Vec<LegDisposition>>,
    pub direction: Option<Direction>,
    pub topic: Option<&'static str>,
    pub outcome: Option<&'static str>,
    /// For a lifecycle rule: the one event kind it takes
    /// ([`LifecycleEvent::kind`]); `None` takes every kind.
    pub lifecycle: Option<&'static str>,
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
            timer_service: None,
            call_state: None,
            leg_state: None,
            leg_disposition: None,
            direction: None,
            topic: None,
            outcome: None,
            lifecycle: None,
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
    /// An attempt opened ([`LifecycleEvent::AttemptStarted`]).
    pub fn attempt_started() -> Self {
        Self { lifecycle: Some("attempt-started"), ..Self::base(MatchKind::Lifecycle) }
    }
    /// An attempt ended ([`LifecycleEvent::AttemptEnded`]).
    pub fn attempt_ended() -> Self {
        Self { lifecycle: Some("attempt-ended"), ..Self::base(MatchKind::Lifecycle) }
    }
    /// A leg ended ([`LifecycleEvent::LegEnded`]).
    pub fn leg_ended() -> Self {
        Self { lifecycle: Some("leg-ended"), ..Self::base(MatchKind::Lifecycle) }
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
    /// Match any [`TimerType::Service`] firing owned by `service_id` (any key).
    /// See [`Self::timer_service`].
    pub fn service_timers(mut self, service_id: MachineId) -> Self {
        self.timer_service = Some(service_id);
        self
    }
    pub fn call_state(mut self, s: CallModelState) -> Self {
        self.call_state.get_or_insert_with(Vec::new).push(s);
        self
    }
    /// The source leg's state ([`call::Leg::state`]), read off its facts as
    /// the event finds them, is one of `s`.
    pub fn leg_states(mut self, s: &[LegState]) -> Self {
        self.leg_state = Some(s.to_vec());
        self
    }
    /// The source leg's INVITE outcome ([`call::Leg::disposition`]) is `d`
    /// (cumulative: one of every `d` given).
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
                | (MatchKind::Lifecycle, CallEvent::Lifecycle(_))
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
                MatchKind::Request => ctx.request().map(|r| r.method().to_string()),
                MatchKind::Response => ctx.response().map(|r| r.cseq().method().to_string()),
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
                if !self.status.accepts(r.status()) {
                    return false;
                }
            }
        }

        if let Some(tts) = &self.timer_types {
            match ctx.timer_type() {
                Some(t) if tts.contains(t) => {}
                _ => return false,
            }
        }
        if let Some(sid) = &self.timer_service {
            match ctx.timer_type() {
                Some(TimerType::Service { service_id, .. }) if service_id == sid => {}
                _ => return false,
            }
        }

        if let Some(states) = &self.call_state {
            if !states.contains(&ctx.call.state()) {
                return false;
            }
        }
        if let Some(states) = &self.leg_state {
            match ctx.source_leg() {
                Some(leg) if states.contains(&leg.state()) => {}
                _ => return false,
            }
        }
        if let Some(disps) = &self.leg_disposition {
            match ctx.source_leg() {
                Some(leg) if disps.contains(&leg.disposition()) => {}
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
        if let Some(lifecycle) = self.lifecycle {
            if !matches!(ctx.event, CallEvent::Lifecycle(e) if e.kind() == lifecycle) {
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
///
/// A rule may belong to a per-call **machine** (ADR-0016 X1): it is then a
/// selection candidate only when that machine's cursor is in [`Self::active_states`]
/// (see `executor::pick_ranked`). Machine-less ("core") rules — built via
/// [`Self::core`] — leave `machine`/`active_states`/`transitions` unset and are
/// always candidates. The `transitions` it declares are the `(from, to)` edges
/// its handle may cause via `SetState` (checked + diagram-generated in later
/// slices).
#[derive(Clone)]
pub struct RuleDefinition {
    pub id: &'static str,
    pub layer: u8,
    pub overrides: &'static [&'static str],
    pub matcher: Match,
    pub handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    /// Owner machine, or `None` for a machine-less core rule.
    pub machine: Option<MachineId>,
    /// States (within `machine`) in which this rule is a candidate.
    pub active_states: &'static [StateLabel],
    /// Declared `(from, to)` transition edges this rule's handle may cause.
    pub transitions: &'static [(StateLabel, StateLabel)],
    /// The **tracked side effects** this rule's handle may emit (ADR-0016 X9).
    /// Required for a machine-bound (`sm_rule!`) rule; empty for a core rule. The
    /// executor verifies the handler's emitted actions are a subset of these *by
    /// [`EffectKind`]* (cursor moves + bookkeeping are auto-allowed).
    pub effects: &'static [Effect],
    /// A teardown rule stays a selection candidate on a `Terminating` /
    /// `Terminated` call for an asynchronous trigger — a timer fire, a
    /// transaction timeout, an internal-event fold
    /// ([`CallEvent::is_asynchronous_trigger`]). Every other rule is absorbed
    /// there: a call already going away makes no forward progress on its own
    /// clock. A peer's message and a CANCEL keep their per-rule filters.
    pub teardown: bool,
}

impl RuleDefinition {
    /// A machine-less ("core") rule: always a selection candidate, regardless of
    /// any machine cursor. Every core rule is built through this; only
    /// `sm_rule!`-generated service rules populate the machine fields directly.
    pub fn core(
        id: &'static str,
        layer: u8,
        overrides: &'static [&'static str],
        matcher: Match,
        handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    ) -> Self {
        Self {
            id,
            layer,
            overrides,
            matcher,
            handle,
            machine: None,
            active_states: &[],
            transitions: &[],
            effects: &[],
            teardown: false,
        }
    }

    /// Mark this a teardown rule ([`Self::teardown`]): it still runs on a
    /// going-away call for an asynchronous trigger — to resolve a leg, scrub
    /// a spent ledger entry, or reap a wedged teardown.
    pub fn runs_while_terminating(mut self) -> Self {
        self.teardown = true;
        self
    }
}

/// An input a rule could not read, stated as a fact instead of absorbed into a
/// fabricated value. A rule that hits one names the field and why, emits
/// whatever refusal it owns (a reject response, a terminated subscription), and
/// lets the engine log and count the diagnostic — the alternative a rule surface
/// without this seam is forced into is choosing between silence and acting as if
/// the input were fine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleDiagnostic {
    /// The input that did not read (`refer_to`, `destination.port`, …).
    pub field: String,
    /// Why it did not read.
    pub reason: String,
}

impl RuleDiagnostic {
    pub fn unreadable(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self { field: field.into(), reason: reason.into() }
    }
}

impl std::fmt::Display for RuleDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unreadable {}: {}", self.field, self.reason)
    }
}

/// What a rule emits when it handles an event.
#[derive(Debug, Clone, Default)]
pub struct RuleHandleResult {
    pub actions: Vec<RuleAction>,
    /// Inputs this rule refused to read. Carried alongside the actions rather
    /// than in place of them: a rule reports the defect **and** emits the
    /// refusal it owns. The engine logs and counts these; they emit no wire
    /// traffic of their own, so a rule can never route on one.
    pub diagnostics: Vec<RuleDiagnostic>,
    /// The call's record does not rebuild the caller's INVITE the rule read.
    pub unrecoverable: bool,
}

impl RuleHandleResult {
    pub fn new(actions: Vec<RuleAction>) -> Self {
        Self { actions, diagnostics: Vec::new(), unrecoverable: false }
    }

    /// The same actions, carrying `diagnostic` — what a rule returns when an
    /// input did not read and the actions are its refusal.
    pub fn with_diagnostic(mut self, diagnostic: RuleDiagnostic) -> Self {
        self.diagnostics.push(diagnostic);
        self
    }
}

/// Transform applied to a relayed message.
#[derive(Debug, Clone, Default)]
pub struct MessageTransform {
    pub status: Option<u16>,
    pub reason: Option<String>,
    /// Drop the body (+ Content-Type) — bare-180 downgrade (`relayFirst18x`).
    pub drop_body: bool,
    /// Passthrough headers to suppress on the relayed message, e.g.
    /// `Require`/`RSeq` when downgrading a reliable 18x to a bare 180.
    pub remove_headers: Vec<HeaderName>,
    /// Header lines to stamp on the relayed message with replace semantics (the
    /// synthetic-200 / resync-reINVITE Allow + Supported, `promote18xPemTo200`).
    /// Each entry names its own header, so a stamp cannot disagree with the
    /// value it carries.
    pub add_headers: Vec<Entry>,
}

/// The action vocabulary. The basic-B2BUA subset is exercised now; the trailing
/// cluster (provisional/prack/notify/reinvite/sdp/policy/refer) is defined for
/// the deferred 18x/transfer service rules and is unused until they land.
#[derive(Debug, Clone)]
pub enum RuleAction {
    RelayToPeer {
        transform: MessageTransform,
    },
    RelayToLeg {
        leg_id: String,
        transform: MessageTransform,
    },
    Respond {
        status: u16,
        reason: String,
        body: Vec<u8>,
        content_type: Option<String>,
    },
    /// Hold the current received non-INVITE request for a final a service
    /// gives on a later turn ([`RuleAction::AnswerHeld`]); nothing answers it
    /// now. A held request the service has not answered by its deadline, the
    /// originator's Timer F less T2, is answered `500` by the platform
    /// (RFC 4320 §4.2, ADR-0055 §2).
    HoldRequest,
    /// Answer the request held on `leg_id` whose sender numbered it `cseq`
    /// ([`RuleAction::HoldRequest`]) with `status`, carrying `body`. A request
    /// no longer held (answered, or its deadline passed) draws nothing.
    AnswerHeld {
        leg_id: String,
        cseq: i64,
        status: u16,
        reason: String,
        body: Vec<u8>,
        content_type: Option<String>,
    },
    /// Refuse the current INVITE that meets glare, or UPDATE over an open
    /// offer, in place (RFC 3261 §14.2, RFC 3311 §5.2), by [`GlareRefusal`].
    RefuseGlare {
        refusal: GlareRefusal,
    },
    /// ACK a leg's confirmed dialog. `body` rides the ACK (delayed-offer answer,
    /// RFC 3261 §13.2.2.4 / RFC 3264 §4) — `None` for the ordinary bare ACK,
    /// which carries no body and no Content-Type.
    AckLeg {
        leg_id: String,
        body: Option<Body>,
    },
    /// ACK the INVITE final the current response carries on `leg_id`'s
    /// confirmed dialog for a round the record does not hold (ADR-0047): the
    /// dialog's sequence is raised to the response's first (RFC 3261
    /// §12.2.1.1); a 2xx is ACKed end to end and its ACK retained for a repeat
    /// (§13.2.2.4), a non-2xx hop by hop on the response's own branch (§17.1.1.3).
    AckLostRound {
        leg_id: String,
    },
    /// Release the fork straggler whose 2xx the current event carries on a
    /// b-leg another fork already confirmed (RFC 3261 §13.2.2.4): ACK that
    /// 2xx, and BYE its dialog the first time it arrives.
    ReleaseStraggler2xx {
        leg_id: String,
    },
    ConfirmDialog {
        leg_id: String,
    },
    /// Map the callee early dialog `(b_leg_id, b_tag)`, one no provisional
    /// showed the caller, to a FRESH a-facing To-tag, so its 2xx opens a caller
    /// dialog of its own (RFC 3261 §12.1.2). A dialog already mapped keeps its
    /// tag.
    MapUnshownDialog {
        b_leg_id: String,
        b_tag: String,
    },
    Merge {
        leg_a: String,
        leg_b: String,
    },
    Split {
        leg_id: String,
    },
    CreateLeg {
        destination: (String, u16),
        new_ruri: Option<String>,
        /// Identity rewrites (ADR-0017): override the new leg's From/To **URI**
        /// (the from/to numbers); tags stay B2BUA-owned. `None` keeps A's URIs.
        new_from: Option<String>,
        new_to: Option<String>,
        callback_context: Option<String>,
        /// Body override for the C INVITE (REFER transfer held SDP). `None`
        /// keeps A's INVITE body (its peer's); `Some(body)` replaces it, empty
        /// bytes dropping it.
        body_override: Option<Body>,
        /// Header overrides applied to the C INVITE (`update_headers` from the
        /// /call/refer allow). `(name, Some(value))` sets, `(name, None)` removes.
        header_updates: Vec<(String, Option<String>)>,
        /// Lines stated on the built INVITE only where it carries none of
        /// the name, `(name, lines)` — the INVITE as minted, privacy
        /// concealment and every relay applied.
        header_adds: Vec<(String, Vec<String>)>,
        /// Leg role (ADR-0014/0016). `None` defaults to [`LegKind::Destination`];
        /// a service parks an unadopted media leg via `Some(LegKind::Media)`. The
        /// leg's `adopted` flag derives from this kind (`is_adopted`), so a media
        /// leg is gated out of the generic relay-to-peer fallback.
        kind: Option<LegKind>,
        /// The attempt this leg dials, its ring deadline among it: a destination
        /// or transfer-target leg opens one ([`AttemptOpening`]), the leg joining
        /// it; a leg a service dials for its own use (a media leg) opens none.
        attempt: Option<AttemptOpening>,
    },
    /// End b-leg `leg_id` as its state asks: a confirmed dialog is BYE'd, a
    /// pending INVITE CANCELled (RFC 3261 §9.1), and the leg terminated.
    /// `headers` are application headers the rule states on that BYE or
    /// CANCEL, verbatim — the RFC 3326 `Reason` of a release of its own. A BYE
    /// stating no `Reason` in a turn no peer's BYE or CANCEL drives carries
    /// the deployment's `own_release_reason`; a media leg gets none.
    DestroyLeg {
        leg_id: String,
        headers: Vec<(String, String)>,
    },
    CancelLeg {
        leg_id: String,
    },
    /// CANCEL a relayed, still-unanswered **re-INVITE** client transaction on
    /// `leg_id`'s dialog (RFC 3261 §9.1) — transaction-scoped: no leg state or
    /// disposition change, the established dialog and the call stay up. Builds
    /// the CANCEL from the INVITE the dialog's record holds (same
    /// branch / Route set / wire destination as the re-INVITE) and marks the
    /// matching pending-relay snapshot (`outbound_cseq`) cancelled so the
    /// peer's eventual final is resolved locally instead of relayed: the
    /// originator already holds its final — the txn layer's 487 to its own
    /// CANCEL here, this stack's reject under [`Self::RejectPendingReinvite`].
    CancelPendingReinvite {
        leg_id: String,
        outbound_cseq: i64,
    },
    /// End a relayed re-INVITE still pending on `leg_id`'s dialog under
    /// `outbound_cseq` on BOTH faces, transaction-scoped: answer its
    /// originator with the locally authored `status` final, and CANCEL the
    /// relayed request toward its target as [`Self::CancelPendingReinvite`]
    /// does. Neither dialog moves (RFC 3261 §14.1).
    RejectPendingReinvite {
        leg_id: String,
        outbound_cseq: i64,
        status: u16,
        reason: String,
    },
    /// Resolve a CANCELled relayed re-INVITE's final response locally: drop the
    /// cancelled pending-relay snapshot (`outbound_cseq`) from `leg_id`'s
    /// dialog. Never relayed — the txn layer answered the originator when the
    /// CANCEL matched.
    ResolveCancelledReinvite {
        leg_id: String,
        outbound_cseq: i64,
    },
    /// Arm (or re-arm, replacing the key's instance) a per-call timer; a
    /// service arms its own watchdog under a [`TimerType::Service`] key.
    ///
    /// A [`TimerType::Keepalive`] is held to one `keepalive_interval_sec`
    /// (`call::helpers::cap_keepalive_fire_at`): a larger `delay` trips a debug
    /// assertion and is clamped in release. Every other timer type keeps its
    /// full `delay`.
    ScheduleTimer {
        key: TimerKey,
        delay: TimerDelay,
    },
    /// Disarm the per-call timer `key`.
    CancelTimer {
        key: TimerKey,
    },
    CancelAllTimers,
    /// Hard-terminate every leg and the call, no wire traffic: the firing
    /// rule owns any final or BYE already sent. `cause` and `by_leg` are the
    /// call's termination record (`Call::termination`, written once by the
    /// first termination).
    TerminateCall {
        cause: TerminationCause,
        by_leg: Option<String>,
    },
    /// Graceful teardown of every unresolved leg, then `Terminating`.
    /// `reason` is a label — an RFC 3326 `SIP;cause=…` value rides the minted
    /// BYEs, anything else is not emitted, and a BYE of a release no peer
    /// asked for carries the deployment's `own_release_reason` where it states
    /// no other `Reason`; `cause` and `by_leg` are the
    /// call's termination record (`Call::termination`, written once by the
    /// first termination: who ended the call and why).
    BeginTermination {
        reason: Option<String>,
        cause: TerminationCause,
        by_leg: Option<String>,
    },
    /// End `leg_id` as `end` records ([`LegEnd`]): its ladders retire, the
    /// requests relayed toward it are answered by this stack, and the early
    /// dialogs an ended leg leaves open retire.
    TerminateLeg {
        leg_id: String,
        end: LegEnd,
    },
    AddCdrEvent {
        event_type: CdrEventType,
        leg_id: String,
        status_code: Option<i64>,
        reason: Option<String>,
    },
    /// A service left out text it was stated because the wire cannot carry it
    /// whole (`sip_message::write_check`), `detail` naming it: the stack
    /// reports it as the model defect a skip of its own is.
    ReportStatedTextSkipped {
        detail: String,
    },
    DeactivateRule {
        rule_id: String,
    },
    /// Move a machine's cursor (ADR-0016 X4): write `call.sm_cursors[machine] =
    /// to`. The sole writer of the cursor map. The legality of the resulting
    /// `(from, to)` edge against the emitting rule's declared `transitions` is
    /// checked in the executor (debug panic / release log-and-proceed).
    SetState {
        machine: MachineId,
        to: StateLabel,
    },
    /// Deactivate a machine: remove its cursor from `call.sm_cursors` (ADR-0016
    /// X9). The declarative inverse of [`Self::SetState`] and the only other
    /// writer of the cursor map; a rule emits it to reach its machine's terminal
    /// (`[*]`) state. Idempotent — clearing an absent cursor is a no-op. Like
    /// `SetState`, it is a cursor move ([`EffectKind::CursorMove`]), not a tracked
    /// side effect: the deactivation is drawn as the transition to `[*]`, declared
    /// via the rule's `transitions`, not its `effects`.
    ClearState {
        machine: MachineId,
    },
    /// Originate an in-dialog request toward a leg's confirmed dialog. Method is
    /// restricted to the body-bearing/keepalive subset (OPTIONS / INFO / UPDATE /
    /// MESSAGE — plus BYE/INVITE/PRACK/NOTIFY for internal callers). `body` is an
    /// **opaque** payload (MSCML rides here as `application/mediaservercontrol+xml`);
    /// its `content_type` defaults to `application/sdp` when none is given.
    ///
    /// `headers` forwards arbitrary application headers verbatim onto the
    /// re-originated request (mirrors [`RuleAction::ServiceHttpRequest`]'s header
    /// list). This is the seam a service uses to **reconstruct a deferred relay**:
    /// e.g. a deferred User-to-User relay stashes the inbound INFO's
    /// `User-To-User` header across an async decision, then re-emits it toward the peer here with
    /// `headers: [("User-To-User", value)]`. The B2BUA rebuilds
    /// every dialog/transport header itself (Via/CSeq/From/To/Call-ID/Contact), so
    /// only application headers belong here. `Content-Type`/`Content-Length` are
    /// owned by `body` and are ignored if listed (never duplicated).
    SendRequestToLeg {
        leg_id: String,
        method: String,
        body: Option<Body>,
        headers: Vec<(String, String)>,
    },
    /// Broker an unadopted leg's SDP onto the a-leg as an **unreliable** `1xx`
    /// (RFC 3262 §3 early media). Only the a-leg is answered. The early media rides
    /// an early dialog of its own (RFC 3261 §12.1.2): under `to_tag`, else one the
    /// B2BUA picks (its own where the caller saw none, else fresh). A callee answers
    /// under another tag, its 2xx ending that early dialog (RFC 3264 §8).
    /// `p_early_media` stamps the RFC 5009 `P-Early-Media` header when set.
    SendProvisionalToLeg {
        leg_id: String,
        status: u16,
        reason: String,
        body: Option<Body>,
        to_tag: Option<String>,
        p_early_media: Option<String>,
    },
    // ── relayFirst18xTo180 (SERVICE_LAYER) ──────────────────────────────────
    /// Cache an SDP body on a b-leg dialog (selected by `b_tag`) for later
    /// substitution into the 200 OK toward alice (`fake-prack`).
    CacheSdpOnLegDialog {
        leg_id: String,
        b_tag: String,
        body: Vec<u8>,
    },
    /// Register the early dialog the current event's provisional establishes
    /// on `leg_id` under the callee tag `b_tag` (RFC 3261 §12.1.2: its
    /// Contact and recorded route), when the provisional is relayed to no one.
    /// Idempotent per tag; only the event's own response can seed it. A
    /// reliable provisional's dialog is the engine's, opened as it is
    /// recorded.
    TrackEarlyDialog {
        leg_id: String,
        b_tag: String,
    },
    /// Stage `body` into `call.policy_update_body` so the response relay path
    /// substitutes it into the next relayed body (`fake-prack` 200-OK SDP).
    SetPolicyUpdateBody {
        body: Vec<u8>,
    },
    /// Bare-180 downgrade relay (`relayFirst18xTo180`): mint an a-facing To-tag
    /// on the FIRST 18x (the executor owns the IdGen) — or reuse the stored one
    /// on a later 18x the `Relay18xMessages` policy relays again (the caller
    /// keeps ONE stable early-dialog identity) — the caller-facing dialog of
    /// that tag fronts this b-leg dialog — record `stored_a_tag` +
    /// `first_relayed` (+ the upstream status value for `ONE_PER_VALUE`
    /// dedupe), and relay the current 1xx to the caller as a bare 180 (no body
    /// / Require / RSeq). The fronting dialog is the single source the relay
    /// path resolves the tag from.
    RelayFirstBare180 {
        leg_id: String,
        b_tag: String,
    },
    // ── promote18xPemTo200 (SERVICE_LAYER) ──────────────────────────────────
    /// Originate a re-INVITE on `leg_id` (here always the a-leg) carrying `body`
    /// as the new offer plus `add_headers` (Allow/Supported), CSeq =
    /// the dialog's `our_cseq` + 1. Used to resync Alice when bob's final SDP differs
    /// from the early-media SDP that was promoted into the synthetic 200 OK.
    SendReinvite {
        leg_id: String,
        body: Option<Body>,
        add_headers: Vec<Entry>,
    },
    /// Overwrite the per-call PEM runtime slice (`None` → pre-promotion state).
    SetPromotePem {
        state: Option<call::PromotePemState>,
    },
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
    ReferAsyncHttp {
        request: serde_json::Value,
    },
    /// The **generic, service-authorable** async-HTTP callback (the seam that
    /// generalizes [`ReferAsyncHttp`](Self::ReferAsyncHttp)/
    /// [`FailureAsyncHttp`](Self::FailureAsyncHttp)). A custom service emits
    /// this mid-dialog to POST/GET a logical adaptation endpoint with a
    /// **binary** body; the host-injected `AdaptationHttpPort` maps `endpoint`
    /// (a logical/relative path) onto its base URL, fires the request under
    /// `timeout_ms` (host default if `None`), and folds the response back as a
    /// `service-http-result` internal event whose raw entity bytes ride
    /// [`CallEvent::InternalEvent::body`] verbatim (binary-safe — never a
    /// character string). The `correlation_id` is service-minted and echoed in
    /// the result `payload`, so a consuming rule's `Match::internal_event()
    /// .topic("service-http-result").filter(..)` disambiguates concurrent
    /// in-flight requests. If no port is injected the interpreter still folds an
    /// `outcome:"error"` result back. A result lost with the node that sent the
    /// request is folded back at the request's answer deadline wherever the
    /// call is served then, as `outcome:"error"` with `"error":"no_answer"`; a
    /// result landing past that deadline reaches no rule (ADR-0039).
    ServiceHttpRequest {
        /// Service-minted opaque id echoed back in the result `payload`.
        correlation_id: String,
        /// Logical/relative request path (the host `AdaptationHttpPort` maps it
        /// onto its base URL); may carry a query string, like `HttpRequest.path`.
        endpoint: String,
        /// e.g. `"POST"` / `"GET"`.
        method: String,
        /// Request headers as ordered `(name, value)` pairs.
        headers: Vec<(String, String)>,
        /// The **binary** request payload — an opaque `Vec<u8>`, never coerced
        /// through a character string.
        body: Vec<u8>,
        /// Appended as a `Content-Type` request header when set.
        content_type: Option<String>,
        /// Per-request budget; the host `AdaptationHttpPort::default_timeout`
        /// when `None`. Independent of `call_control_timeout_ms`.
        timeout_ms: Option<u64>,
    },
    /// Overwrite the per-call REFER transfer runtime slice (`None` clears it —
    /// the terminal path; mirrors `SetPromotePem`).
    SetTransfer {
        state: Option<call::TransferState>,
    },
    // ── b-leg failover (/call/failure) ───────────────────────────────────────
    /// Kick the async `/call/failure` decision: push a `FailureAsyncHttp`
    /// fire-and-forget effect carrying the request JSON. The router interpreter
    /// calls `decision.call_failure` then re-enters via a `call-failure-result`
    /// internal event. The emitting rule restates the failure image with it
    /// ([`crate::failure_image::no_failure_image`] when no peer final failed).
    FailureAsyncHttp {
        request: serde_json::Value,
    },
    // ── subscribed release events (call_release) ────────────────────────────
    /// Kick the async `call_release` consult for a **subscribed** internal
    /// release event (max-call-duration first): push a `ReleaseAsyncHttp`
    /// fire-and-forget effect carrying the event-scoped request JSON
    /// (`callback_context`, `event`). The router interpreter attaches the
    /// call-scoped `CallSnapshot`, calls `decision.call_release`
    /// (deadline-bounded — a hung consult must never wedge the established
    /// call), then re-enters via a `call-release-result` internal event
    /// (`release` → local teardown; `reroute` → the established-call reroute
    /// treatment). Engine error / timeout folds `release`, so local teardown
    /// stays the fail-safe.
    ReleaseAsyncHttp {
        request: serde_json::Value,
    },
    /// Record the decision-declared release-event **subscriptions** on the
    /// call (replaces the previous set — the latest applied `Route` owns it).
    /// The async (re)route folds back through the rule layer, so this is how
    /// a failover/reroute route's `subscriptions` reach the replicated call
    /// (`apply_route` records the initial route's directly).
    SetSubscriptions {
        events: Vec<call::ReleaseEventKind>,
    },
    /// Record a decision a service's own rule applies on the call's decision
    /// log (`call::helpers::mark_decision`). A CDR event is stamped with the
    /// ordinal current when its action runs, so the mark goes ahead of the
    /// events it governs — first in the list, by convention; the messages a
    /// turn sends are stamped with the ordinal the call holds when the turn
    /// ends, whatever the mark's place. The engine's own decisions never need
    /// it: the initial route and reject mark directly, and an async fold is
    /// marked where it lands, before any rule reads it.
    MarkDecision {
        kind: call::DecisionKind,
        /// The leg whose event the decision answers; see `DecisionMark`.
        leg_id: Option<String>,
        label: Option<String>,
    },
    /// Overwrite the per-call established-call **reroute** runtime slice
    /// (`None` clears it — completion / rollback; mirrors [`Self::SetTransfer`]).
    SetReroute {
        state: Option<call::RerouteState>,
    },
    /// Overwrite the call's feature activations from a (re)route decision. The
    /// initial route applies features inside `apply_route`; the async failover
    /// route folds back through the rule layer, so this is how the reroute's
    /// features reach the call (failover/initial-route parity).
    SetFeatures {
        features: FeatureActivations,
    },
    /// Replace the call's stated headers (`features.stated_headers`) with
    /// `headers` from this turn on, `None` clearing them: what a decision that
    /// applies no route states. A call with no features takes none.
    SetStatedHeaders {
        headers: Option<call::features::StatedHeaders>,
    },
    /// Merge per-service ext slices into `call.ext` (the failover-route
    /// counterpart of `apply_route`'s `service_ext` seeding). `null` values
    /// clear the slice, mirroring `set_call_ext`.
    MergeCallExt {
        ext: ExtMap,
    },
    /// Replace the call's admission set on the call limiter with `entries`:
    /// one admit of the whole set under the call's next change number, all or
    /// none, checked net of the set the call holds; a cap refusal keeps that
    /// set. With `moves_call` the sending turn moves the call onto `entries`:
    /// it runs on them whatever the outcome, and runs uncounted while the
    /// limiter does not hold them all. The router re-enters with a [`LIMITER_ADMIT_RESULT`](crate::fold_payload::LIMITER_ADMIT_RESULT) internal event
    /// whose outcome is the admit's and whose payload echoes `correlation_id`
    /// beside the admit's report; the call's limiter state already states the
    /// outcome ([`RuleCall::limiter_held`]) when a rule reads the event. The
    /// call owes its release once the request left; a result landing on a
    /// gone call releases the key. A result lost with the node that sent the
    /// admit is folded back at its answer deadline as `unavailable`; a result
    /// landing past that deadline is applied to the call and reaches no rule
    /// (ADR-0039).
    ReplaceAdmissionSet {
        /// Service-minted opaque id echoed back in the result `payload`.
        correlation_id: String,
        /// The whole set the call is to hold.
        entries: Vec<call::LimiterEntry>,
        /// The call runs on `entries` from this turn whatever the outcome
        /// (true), or keeps running on what it runs on (false: the service
        /// waits for the result).
        moves_call: bool,
    },
    /// Synthesize a final failure response on the a-leg INVITE server txn
    /// (the terminate-after-`/call/failure` path — relay the b-leg failure to A
    /// once the backend declines to fail over). Reuses the a-dialog tag.
    RelayFailureToALeg {
        status: u16,
        reason: String,
    },
    /// Author a decision-layer **Reject** or **Redirect** treatment on the a-leg
    /// INVITE server txn (ADR-0017 failover path). `header_updates` add
    /// non-structural headers (e.g. `Reason:`, RFC 3326); `contacts` (`uri`, `q`)
    /// render `Contact:` headers for a 3xx redirect. Distinct from
    /// [`RelayFailureToALeg`](Self::RelayFailureToALeg), which relays the b-leg's
    /// own failure with the B2BUA Contact.
    RespondToALeg {
        status: u16,
        reason: String,
        header_updates: Vec<(String, Option<String>)>,
        contacts: Vec<(String, Option<f32>)>,
    },
    /// **A-side fork-confirm** (RFC 3261 §12.1 forked-request dialog
    /// establishment + RFC 3264 §5.1 one-answer-per-dialog): answer the a-leg
    /// INVITE with a final **2xx** under a *fresh* a-facing To-tag that becomes
    /// the confirmed a-dialog, **superseding** any early-media dialog the caller
    /// saw on a prior `18x`.
    ///
    /// An early-media announcement callflow can answer ONE caller INVITE in two
    /// stages with two *different* To-tags: a `183` (the media server's SDP, tag
    /// A1) then a `200` (SDP-B, tag A2 ≠ A1). A1 and A2 carry different SDP
    /// answers to the caller's single offer, so keeping A1 for both would put two
    /// answers in one dialog (RFC 3264 §5.1 violation). This action delivers the
    /// RFC-correct tag change: it (i) mints a fresh a-facing To-tag A2 (or uses
    /// `to_tag` verbatim), (ii) sets/replaces the a-dialog `local_tag` to A2 and
    /// confirms the a-leg, and (iii) relays the final/SDP under A2. Only a `2xx`
    /// establishes a dialog — a non-2xx status is a no-op (the abandoned early
    /// dialog / the ADR-0022 unanswered-a-leg funnel own the failure paths). The
    /// body's `content_type` defaults to `application/sdp`; `header_updates`
    /// follow the non-structural set/remove discipline of
    /// [`Self::RespondToALeg`]; `relayed` is the delivered callee final's lines
    /// that ride onto this answer (RFC 3261 §16.6), exactly as the plain relay
    /// would carry them.
    ///
    /// Caveat (unchanged in sip-txn): after this 2xx, a late CANCEL's autonomous
    /// 487 still carries the *pinned* early tag A1 — harmless, as it matches the
    /// caller's abandoned early dialog A1.
    AnswerALegNewDialog {
        status: u16,
        reason: String,
        body: Option<Body>,
        /// Explicit a-facing To-tag A2. `None` ⇒ mint a fresh one (guaranteed
        /// distinct from any prior early-media tag).
        to_tag: Option<String>,
        /// Non-structural header sets/removes (structural keys are ignored),
        /// same discipline as [`Self::RespondToALeg`]. An entry naming a header
        /// owns it over a relayed line of the same name.
        header_updates: Vec<(String, Option<String>)>,
        /// The callee final this answer delivers, as the lines of it that ride.
        /// [`RelayedFinal::none`] when the B2BUA answers on its own behalf.
        relayed: RelayedFinal,
    },
}

impl RuleAction {
    /// Arm `key` after `delay` ([`RuleAction::ScheduleTimer`]).
    pub fn arm(key: TimerKey, delay: TimerDelay) -> Self {
        RuleAction::ScheduleTimer { key, delay }
    }
    /// Disarm `key` ([`RuleAction::CancelTimer`]).
    pub fn cancel(key: TimerKey) -> Self {
        RuleAction::CancelTimer { key }
    }
}

/// The context a rule sees, built from the event and the call: the call through the
/// narrow [`RuleCall`] view, the full `Call` staying on the framework's side (ADR-0020 X8).
pub struct RuleContext<'a> {
    pub call: RuleCall<'a>,
    pub call_ref: &'a str,
    pub event: &'a CallEvent,
    pub source_leg_id: &'a str,
    pub direction: Direction,
    pub now_ms: i64,
    pub config: &'a B2buaConfig,
    /// The dialog-level ladder this event's ACK or PRACK discharged before the
    /// rules ran (ADR-0032 X4) — the engine already retired its timers and
    /// retained emission; a rule reads the fact and owns none of the plumbing.
    /// `None` for every other event, and for an ACK or PRACK that named no
    /// live ladder (a retransmitted ACK, a repeat PRACK).
    pub discharged: Option<&'a Obligation>,
    /// What the engine's dialog gate resolved the event's message to before
    /// the rules ran: the dialog it names on the source leg, and the relay a
    /// response answers. Empty for any other event.
    pub resolved: &'a call::helpers::Resolved,
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
    pub fn timer_type(&self) -> Option<&TimerType> {
        self.timer_key().map(|key| &key.timer_type)
    }
    /// For a lifecycle delivery: the event delivered.
    pub fn lifecycle(&self) -> Option<&LifecycleEvent> {
        match self.event {
            CallEvent::Lifecycle(e) => Some(e),
            _ => None,
        }
    }
    /// For a timer fire: the key of the timer that fired.
    pub fn timer_key(&self) -> Option<&TimerKey> {
        match self.event {
            CallEvent::Timer { key, .. } => Some(key),
            _ => None,
        }
    }
    /// For a fired [`TimerType::Service`] timer: `(owning service, key)`. `None`
    /// for every other event (including core timer fires). The branch point for
    /// a rule that funnels several watchdog keys through one
    /// [`Match::service_timers`] matcher.
    pub fn service_timer_key(&self) -> Option<(&MachineId, &str)> {
        match self.timer_type() {
            Some(TimerType::Service { service_id, key }) => Some((service_id, key)),
            _ => None,
        }
    }
    /// For a `Cancelled` event: did the CANCEL match an **in-dialog** INVITE
    /// server transaction (a re-INVITE — its `To` carried a tag)? `false` for
    /// the initial INVITE and for every other event kind. RFC 3261 §9: this is
    /// the discriminator between "caller abandoned the call" and "caller
    /// abandoned one renegotiation".
    pub fn cancelled_in_dialog(&self) -> bool {
        matches!(self.event, CallEvent::Cancelled { in_dialog: true, .. })
    }
    /// For a `Cancelled` event: the CANCEL's own header lines, empty for every
    /// other event. The transaction layer answered the CANCEL, so this is the
    /// only place the canceller's `Reason` (RFC 3326 §2) is ever stated.
    pub fn cancelled_headers(&self) -> &'a [sip_message::SipHeader] {
        match self.event {
            CallEvent::Cancelled { headers, .. } => headers,
            _ => &[],
        }
    }
    /// For a `Cancelled` event: the CSeq number of the INVITE transaction the
    /// CANCEL matched (the canceller's own CSeq space — i.e. the relayed
    /// pending request's `inbound_cseq`).
    pub fn cancelled_invite_cseq(&self) -> Option<i64> {
        match self.event {
            CallEvent::Cancelled { invite_cseq, .. } => invite_cseq.map(i64::from),
            _ => None,
        }
    }
    /// Is the leg a [`RuleAction::RelayToPeer`] of the current request would
    /// target in a **relayable** state? `false` exactly when the
    /// relay would go nowhere useful: no peer leg resolves, the peer leg is
    /// `Terminated` (e.g. a failed b-leg whose `/call/failure` reroute is still
    /// pending), or no target dialog resolves (a replacement leg still
    /// `Trying`). An `Early` peer dialog counts as relayable (RFC 3311 §5.1 early-dialog UPDATE is the normal case). Uses
    /// the SAME resolver as the executor's relay path
    /// ([`call::helpers::resolve_relay_peer`]), so match and action never
    /// disagree. Exposed on the context (usable from [`Match::filter`]) so a
    /// service rule can own failover-pending policy; the CORE default is the
    /// `update-peer-unavailable` local 491.
    pub fn peer_relay_ready(&self) -> bool {
        call::helpers::relay_peer_dialog_ready(
            self.call.0,
            self.source_leg_id,
            self.request_dialog_id(),
        )
    }
    /// Whether a [`RuleAction::RelayToPeer`] of the current event reaches any
    /// leg, by the resolver the executor's relay path reads
    /// ([`call::helpers::resolve_relay_peer`]).
    pub fn relay_peer_resolves(&self) -> bool {
        matches!(
            call::helpers::resolve_relay_peer(
                self.call.0,
                self.source_leg_id,
                self.request_dialog_id()
            ),
            call::helpers::RelayPeer::Leg { .. }
        )
    }

    /// Whether the current INVITE meets glare (RFC 3261 §14.1), by the same
    /// predicate the exchange count reads ([`call::helpers::invite_glare`]):
    /// an INVITE transaction open on the source dialog or its relay's, or
    /// the sender's dialog-creating INVITE still without a final.
    pub fn invite_glare(&self) -> bool {
        call::helpers::invite_glare(self.call.0, self.source_leg_id, self.request_dialog_id())
    }

    /// The refusal the current UPDATE draws (RFC 3311 §5.2), if any: 500 over
    /// its sender's UPDATE still awaiting its final, whatever its body; with an
    /// offer, 500 over the sender's open INVITE offer and 491 over this stack's
    /// ([`crate::open_offer::open_offer`]).
    pub fn update_refusal(&self) -> Option<GlareRefusal> {
        let req = self.request().filter(|r| *r.method() == Method::Update)?;
        let source = self.request_dialog_id();
        if call::helpers::peer_update_pending(self.call.0, self.source_leg_id, source) {
            return Some(GlareRefusal::Unanswered);
        }
        req.sdp()?;
        match crate::open_offer::open_offer(self.call.0, self.source_leg_id, source)? {
            crate::open_offer::OpenOffer::Sender => Some(GlareRefusal::Unanswered),
            crate::open_offer::OpenOffer::Stack => Some(GlareRefusal::Crossing),
        }
    }

    /// The §14.2 / RFC 3311 §5.2 refusal the current request draws, if any:
    /// an INVITE meeting glare by [`Self::glare_refusal`], an UPDATE by
    /// [`Self::update_refusal`].
    pub fn offer_refusal(&self) -> Option<GlareRefusal> {
        if let Some(refusal) = self.update_refusal() {
            return Some(refusal);
        }
        let invite = self.request().is_some_and(|r| *r.method() == Method::Invite);
        (invite && self.invite_glare()).then(|| self.glare_refusal())
    }

    /// How the current INVITE's glare is refused (RFC 3261 §14.2): by what is
    /// open on the dialog, the sender's own earlier INVITE first.
    pub fn glare_refusal(&self) -> GlareRefusal {
        let source = self.request_dialog_id();
        if call::helpers::sender_invite_unanswered(self.call.0, self.source_leg_id, source) {
            GlareRefusal::Unanswered
        } else if call::helpers::sender_invite_unacknowledged(
            self.call.0,
            self.source_leg_id,
            source,
        ) {
            GlareRefusal::Unacknowledged
        } else {
            GlareRefusal::Crossing
        }
    }

    /// The leg the event arrived on.
    pub fn source_leg(&self) -> Option<&'a Leg> {
        if self.source_leg_id == self.call.a_leg().leg_id {
            Some(self.call.a_leg())
        } else {
            self.call.b_legs().iter().find(|l| l.leg_id == self.source_leg_id)
        }
    }
    /// The dialog of the source leg the current message names, as the
    /// engine's dialog gate resolved it ([`Self::resolved`]): a request's
    /// To-tag is ours and its From-tag the peer's, a response's the reverse
    /// (RFC 3261 §12.2). `None` for any other event, and when the message
    /// names no dialog of the leg.
    pub fn dialog(&self) -> Option<&'a Dialog> {
        let id = self.resolved.dialog.as_ref()?;
        self.source_leg()?.dialogs.by_tags(&id.our_tag, &id.peer_tag)
    }
    /// The dialog of the source leg the current request names, as the
    /// engine's dialog gate resolved it ([`Self::resolved`]); `None` for any
    /// other event and for a request naming no dialog.
    pub fn request_dialog_id(&self) -> Option<&'a call::DialogId> {
        self.request()?;
        self.resolved.dialog.as_ref()
    }
    /// The dialog a [`RuleAction::RelayToPeer`] of the current request would be
    /// regenerated on, resolved with the SAME resolver as the relay executor
    /// ([`call::helpers::relay_peer_dialog`]) so match and action never
    /// disagree. `None` when no peer leg or dialog resolves.
    pub fn peer_dialog(&self) -> Option<&'a Dialog> {
        call::helpers::relay_peer_dialog(self.call.0, self.source_leg_id, self.request_dialog_id())
            .map(|(_, d)| d)
    }
}

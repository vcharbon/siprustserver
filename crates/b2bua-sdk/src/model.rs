//! The rule-engine core types — port of `RuleDefinition.ts` (declarative
//! `Match`, `RuleContext`, the `RuleAction` vocabulary) reduced to a concrete
//! (non-type-narrowed) shape sufficient for the basic B2BUA. Service-layer
//! ext narrowing and rule composition are deferred with their consumers.

use std::collections::BTreeMap;

use call::features::{FeatureActivations, RelayFirst18xStrategy};
use call::{
    ALegInviteSnapshot, ActivePeer, Call, CallModelState, CdrEvent, CdrEventType, Dialog,
    Direction, ExtMap, Leg, LegDisposition, LegKind, LegState, MachineId, PromotePemState,
    StateLabel, TagMapping, TimerType, TransferState,
};
use sip_message::{Method, SipRequest, SipResponse};

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
                MatchKind::Request => ctx.request().map(|r| r.method.to_string()),
                MatchKind::Response => ctx.response().map(|r| r.cseq.method.to_string()),
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
}

impl RuleDefinition {
    /// A machine-less ("core") rule: always a selection candidate, regardless of
    /// any machine cursor. Every pre-ADR-0016 rule is built through this; only
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
        }
    }
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

/// The category every [`RuleAction`] belongs to (ADR-0016 X9). [`RuleAction::
/// effect_kind`] is a **total** map onto this — the compiler's exhaustiveness
/// check forces each present and future action into exactly one category, so a
/// machine-bound rule's declared [`Effect`]s can be verified against what its
/// handler actually emits (`emitted ⊆ declared`, compared by this kind alone).
///
/// Three kinds are **tracked** (declared by a service rule, rendered on the
/// diagram): [`Self::LegMessage`], [`Self::CallLifecycleCommand`],
/// [`Self::GuardTimer`]. The other two are **not** side effects and are
/// auto-allowed (never declared, never checked): [`Self::CursorMove`] is the
/// transition itself (`SetState`) or machine deactivation (`ClearState`),
/// already drawn as the edge / terminal; [`Self::Bookkeeping`] is a local data
/// write or async kick with no wire output of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectKind {
    LegMessage,
    CallLifecycleCommand,
    GuardTimer,
    CursorMove,
    Bookkeeping,
}

impl EffectKind {
    /// Is this a **tracked** side effect a machine-bound rule must declare? The
    /// cursor-move + bookkeeping kinds are auto-allowed (the executor's effect
    /// drift-check skips them).
    pub fn is_tracked(self) -> bool {
        matches!(
            self,
            EffectKind::LegMessage | EffectKind::CallLifecycleCommand | EffectKind::GuardTimer
        )
    }
}

/// A tracked side effect a machine-bound rule declares it may emit (ADR-0016 X9).
/// Categorised by the **attribution principle** — *who authors the message* —
/// into the three [`EffectKind`]s: a **leg message** (its four wire forms below),
/// a **call-lifecycle command**, or a **guard timer**. Listed in the rule's
/// `effects` (required by `sm_rule!`) and rendered on the generated diagram; the
/// executor's drift-check compares only [`Self::kind`], so the typed payloads are
/// labels for the diagram, not part of the check (a handler targeting a
/// dynamically-named leg still satisfies a `LegMessage`-kind declaration). Every
/// variant carries a free, unenforced `label`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// **Leg message** — a request the service rule **originates** toward a leg
    /// (the MRF INVITE, an MSCML `INFO`, a re-INVITE to A/C, a rule-driven
    /// BYE/CANCEL/ACK). `method` is the canonical [`Method`]; the rule owns the
    /// semantic payload, core fills the mechanical SIP layer.
    Originate { method: Method, label: &'static str },
    /// **Leg message** — a transparent **relay** of the inbound message onward
    /// (peer leg / named leg / a-leg failure); the method is whatever arrived.
    Relay { label: &'static str },
    /// **Leg message** — a final **response** to a leg's server transaction.
    Respond { status: u16, label: &'static str },
    /// **Leg message** — an unreliable **provisional** `1xx` (RFC 3262 early media).
    Provisional { status: u16, label: &'static str },
    /// **Call-lifecycle command** — the one synchronous service → global hop (X3):
    /// the call-lifecycle subset of the action union (`BeginTermination`/
    /// `TerminateCall`/`Merge`/`Split`). The global machine owns any wire messages
    /// it then generates.
    LifecycleCommand { label: &'static str },
    /// **Guard timer** — a service watchdog timer the rule arms or cancels.
    GuardTimer { timer: TimerType, label: &'static str },
}

impl Effect {
    /// The free, unenforced diagram label every variant carries.
    pub fn label(&self) -> &'static str {
        match self {
            Effect::Originate { label, .. }
            | Effect::Relay { label }
            | Effect::Respond { label, .. }
            | Effect::Provisional { label, .. }
            | Effect::LifecycleCommand { label }
            | Effect::GuardTimer { label, .. } => label,
        }
    }

    /// The category this declared effect contributes to the drift-check.
    pub fn kind(&self) -> EffectKind {
        match self {
            Effect::Originate { .. }
            | Effect::Relay { .. }
            | Effect::Respond { .. }
            | Effect::Provisional { .. } => EffectKind::LegMessage,
            Effect::LifecycleCommand { .. } => EffectKind::CallLifecycleCommand,
            Effect::GuardTimer { .. } => EffectKind::GuardTimer,
        }
    }
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
    /// ACK a leg's confirmed dialog. `body` rides the ACK (delayed-offer answer,
    /// RFC 3261 §13.2.2.4 / RFC 3264 §4) — empty for the ordinary bare ACK.
    /// `content_type` defaults to `application/sdp` when a body is present and
    /// none is given; an empty ACK carries no body and no Content-Type.
    AckLeg { leg_id: String, body: Vec<u8>, content_type: Option<String> },
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
        /// Identity rewrites (ADR-0017): override the new leg's From/To **URI**
        /// (the from/to numbers); tags stay B2BUA-owned. `None` keeps A's URIs.
        new_from: Option<String>,
        new_to: Option<String>,
        no_answer_timeout_sec: Option<i64>,
        callback_context: Option<String>,
        /// Body override for the C INVITE (REFER transfer held SDP). `None`
        /// keeps A's INVITE body; `Some(bytes)` replaces it (empty = drop).
        body_override: Option<Vec<u8>>,
        /// Header overrides applied to the C INVITE (`update_headers` from the
        /// /call/refer allow). `(name, Some(value))` sets, `(name, None)` removes.
        header_updates: Vec<(String, Option<String>)>,
        /// Leg role (ADR-0014/0016). `None` defaults to [`LegKind::Destination`];
        /// a service parks an unadopted media leg via `Some(LegKind::Media)`. The
        /// leg's `adopted` flag derives from this kind (`is_adopted`), so a media
        /// leg is gated out of the generic relay-to-peer fallback.
        kind: Option<LegKind>,
    },
    DestroyLeg { leg_id: String },
    CancelLeg { leg_id: String },
    /// CANCEL a relayed, still-unanswered **re-INVITE** client transaction on
    /// `leg_id`'s dialog (RFC 3261 §9.1) — transaction-scoped: no leg state or
    /// disposition change, the established dialog and the call stay up. Builds
    /// the CANCEL from the dialog's cached `pending_invite_txn` handle (same
    /// branch / Route set / wire destination as the re-INVITE) and marks the
    /// matching pending-relay snapshot (`outbound_cseq`) cancelled so the
    /// peer's eventual final is resolved locally instead of relayed (the
    /// originator's own re-INVITE was already 487'd by the txn layer).
    CancelPendingReinvite { leg_id: String, outbound_cseq: i64 },
    /// Resolve a CANCELled relayed re-INVITE's final response locally: drop the
    /// cancelled pending-relay snapshot (`outbound_cseq`) from `leg_id`'s
    /// dialog. Never relayed — the txn layer answered the originator when the
    /// CANCEL matched.
    ResolveCancelledReinvite { leg_id: String, outbound_cseq: i64 },
    /// Arm (or re-arm — same derived id supersedes) a per-call timer. The
    /// persisted id is `timer_type.timer_id(leg_id)`; a service arms its own
    /// watchdog with a [`TimerType::Service`] `(service_id, key)`.
    ScheduleTimer {
        timer_type: TimerType,
        delay_sec: i64,
        leg_id: Option<String>,
    },
    /// Disarm a per-call timer by persisted id. Mint the id with
    /// [`TimerType::timer_id`] (or use [`RuleAction::cancel_timer`]) so it can
    /// never drift from the schedule recipe.
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
    /// Move a machine's cursor (ADR-0016 X4): write `call.sm_cursors[machine] =
    /// to`. The sole writer of the cursor map. The legality of the resulting
    /// `(from, to)` edge against the emitting rule's declared `transitions` is
    /// checked in the executor (debug panic / release log-and-proceed).
    SetState { machine: MachineId, to: StateLabel },
    /// Deactivate a machine: remove its cursor from `call.sm_cursors` (ADR-0016
    /// X9). The declarative inverse of [`Self::SetState`] and the only other
    /// writer of the cursor map; a rule emits it to reach its machine's terminal
    /// (`[*]`) state. Idempotent — clearing an absent cursor is a no-op. Like
    /// `SetState`, it is a cursor move ([`EffectKind::CursorMove`]), not a tracked
    /// side effect: the deactivation is drawn as the transition to `[*]`, declared
    /// via the rule's `transitions`, not its `effects`.
    ClearState { machine: MachineId },
    /// Originate an in-dialog request toward a leg's confirmed dialog. Method is
    /// restricted to the body-bearing/keepalive subset (OPTIONS / INFO / UPDATE /
    /// MESSAGE — plus BYE/INVITE/PRACK/NOTIFY for internal callers). `body` is an
    /// **opaque** payload (MSCML rides here as `application/mediaservercontrol+xml`);
    /// `content_type` defaults to `application/sdp` when a body is present and no
    /// type is given.
    SendRequestToLeg {
        leg_id: String,
        method: String,
        body: Vec<u8>,
        content_type: Option<String>,
    },
    /// Broker an unadopted leg's SDP onto the a-leg as an **unreliable** `1xx`
    /// (RFC 3262 §3 early media — no `Require: 100rel`/`RSeq`). Only the a-leg has
    /// a stored UAS INVITE to answer; other targets are skipped. `to_tag` set ⇒ an
    /// ephemeral forked early dialog (used verbatim, not persisted); absent ⇒ the
    /// B2BUA's own early identity (reuse or mint+persist the a-dialog tag).
    /// `p_early_media` stamps the RFC 5009 `P-Early-Media` header when set.
    SendProvisionalToLeg {
        leg_id: String,
        status: u16,
        reason: String,
        body: Vec<u8>,
        content_type: Option<String>,
        to_tag: Option<String>,
        p_early_media: Option<String>,
    },
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
    /// Bare-180 downgrade relay (`relayFirst18xTo180`): mint an a-facing To-tag
    /// on the FIRST 18x (the executor owns the IdGen) — or reuse the stored one
    /// on a later 18x the `relay18x.messages` policy relays again (the caller
    /// keeps ONE stable early-dialog identity) — seed the tag map for this
    /// b-leg dialog, record `stored_a_tag` + `first_relayed` (+ the upstream
    /// status value for `ONE_PER_VALUE` dedupe), and relay the current 1xx to
    /// the caller as a bare 180 (no body / Require / RSeq). The tag is the
    /// single source the relay path resolves via the tag map.
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
    /// The **generic, service-authorable** async-HTTP callback (the seam that
    /// generalizes [`ReferAsyncHttp`]/[`FailureAsyncHttp`]). A custom service
    /// emits this mid-dialog to POST/GET a logical adaptation endpoint with a
    /// **binary** body; the host-injected `AdaptationHttpPort` maps `endpoint`
    /// (a logical/relative path) onto its base URL, fires the request under
    /// `timeout_ms` (host default if `None`), and folds the response back as a
    /// `service-http-result` internal event whose raw entity bytes ride
    /// [`CallEvent::InternalEvent::body`] verbatim (binary-safe — never a
    /// character string). The `correlation_id` is service-minted and echoed in
    /// the result `payload`, so a consuming rule's `Match::internal_event()
    /// .topic("service-http-result").filter(..)` disambiguates concurrent
    /// in-flight requests. If no port is injected the interpreter still folds an
    /// `outcome:"error"` result back, so the machine is never stranded.
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
    SetTransfer { state: Option<call::TransferState> },
    // ── b-leg failover (/call/failure) ───────────────────────────────────────
    /// Kick the async `/call/failure` decision: push a `FailureAsyncHttp`
    /// fire-and-forget effect carrying the request JSON. The router interpreter
    /// calls `decision.call_failure` then re-enters via a `call-failure-result`
    /// internal event.
    FailureAsyncHttp { request: serde_json::Value },
    // ── subscribed release events (call_release) — newkahneed-009 ───────────
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
    ReleaseAsyncHttp { request: serde_json::Value },
    /// Record the decision-declared release-event **subscriptions** on the
    /// call (replaces the previous set — the latest applied `Route` owns it).
    /// The async (re)route folds back through the rule layer, so this is how
    /// a failover/reroute route's `subscriptions` reach the replicated call
    /// (`apply_route` records the initial route's directly).
    SetSubscriptions { events: Vec<call::ReleaseEventKind> },
    /// Overwrite the per-call established-call **reroute** runtime slice
    /// (`None` clears it — completion / rollback; mirrors [`Self::SetTransfer`]).
    SetReroute { state: Option<call::RerouteState> },
    /// Overwrite the call's feature activations from a (re)route decision. The
    /// initial route applies features inside `apply_route`; the async failover
    /// route folds back through the rule layer, so this is how the reroute's
    /// features reach the call (failover/initial-route parity).
    SetFeatures { features: FeatureActivations },
    /// Merge per-service ext slices into `call.ext` (the failover-route
    /// counterpart of `apply_route`'s `service_ext` seeding). `null` values
    /// clear the slice, mirroring `set_call_ext`.
    MergeCallExt { ext: ExtMap },
    /// Record **already-admitted** limiter holds on the call. The async
    /// failover route's admit runs in the router's fire-and-forget task (the
    /// rule layer is sync); this folds the holds into the replicated call so
    /// termination decrements them. `entries` are `(limiter_id, limit)`.
    RecordLimiterHolds { entries: Vec<(String, i64)>, window: i64 },
    /// Synthesize a final failure response on the a-leg INVITE server txn
    /// (the terminate-after-`/call/failure` path — relay the b-leg failure to A
    /// once the backend declines to fail over). Reuses the a-dialog tag.
    RelayFailureToALeg { status: u16, reason: String },
    /// Author a decision-layer **Reject** or **Redirect** treatment on the a-leg
    /// INVITE server txn (ADR-0017 failover path). `header_updates` add
    /// non-structural headers (e.g. `Reason:`, RFC 3326); `contacts` (`uri`, `q`)
    /// render `Contact:` headers for a 3xx redirect. Distinct from
    /// [`RelayFailureToALeg`], which relays the b-leg's own failure with the
    /// B2BUA Contact.
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
    /// The MRF / RBT early-media callflow answers ONE caller INVITE in two
    /// stages with two *different* To-tags: a `183` (SDP-MRF, tag A1) then a
    /// `200` (SDP-B, tag A2 ≠ A1). A1 and A2 carry different SDP answers to the
    /// caller's single offer, so keeping A1 for both would put two answers in
    /// one dialog (RFC 3264 §5.1 violation). This action delivers the
    /// RFC-correct tag change: it (i) mints a fresh a-facing To-tag A2 (or uses
    /// `to_tag` verbatim), (ii) sets/replaces the a-dialog `local_tag` to A2 and
    /// confirms the a-leg, and (iii) relays the final/SDP under A2. Only a `2xx`
    /// establishes a dialog — a non-2xx status is a no-op (the abandoned early
    /// dialog / the ADR-0022 unanswered-a-leg funnel own the failure paths).
    /// `content_type` defaults to `application/sdp` when a `body` is present;
    /// `header_updates` follow the non-structural set/remove discipline of
    /// [`Self::RespondToALeg`].
    ///
    /// Caveat (unchanged in sip-txn): after this 2xx, a late CANCEL's autonomous
    /// 487 still carries the *pinned* early tag A1 — harmless, as it matches the
    /// caller's abandoned early dialog A1.
    AnswerALegNewDialog {
        status: u16,
        reason: String,
        body: Vec<u8>,
        content_type: Option<String>,
        /// Explicit a-facing To-tag A2. `None` ⇒ mint a fresh one (guaranteed
        /// distinct from any prior early-media tag).
        to_tag: Option<String>,
        /// Non-structural header sets/removes (structural keys are ignored),
        /// same discipline as [`Self::RespondToALeg`].
        header_updates: Vec<(String, Option<String>)>,
    },
    /// RFC 3261 §13.3.1.4 — retransmit the a-leg INVITE **2xx** toward the caller
    /// while its ACK is missing. Sent **raw** (the a-leg INVITE server txn is
    /// already `Completed` and would DROP a second final via the txn layer), with
    /// the dialog's confirmed To-tag + the cached answer SDP, so it is a faithful
    /// copy of the original 200 the caller can ACK. No-op if the a-leg dialog is
    /// not yet confirmed.
    RetransmitALeg2xx,
}

impl RuleAction {
    /// [`RuleAction::CancelTimer`] with the id minted from the canonical
    /// schedule recipe ([`TimerType::timer_id`]) — the symmetric disarm for any
    /// `ScheduleTimer { timer_type, leg_id }` (core or service-owned).
    pub fn cancel_timer(timer_type: &TimerType, leg_id: Option<&str>) -> Self {
        RuleAction::CancelTimer { id: timer_type.timer_id(leg_id) }
    }

    /// The single [`EffectKind`] this action contributes (ADR-0016 X9). Total by
    /// construction: a new `RuleAction` variant will not compile until it is
    /// categorised here, so a machine-bound rule's declared `effects` can be
    /// verified against what its handler actually emits. Categorisation follows
    /// the **attribution principle** — a message the rule authors toward a leg is
    /// a [`EffectKind::LegMessage`] (including the leg-creating INVITE and a
    /// rule-driven BYE/CANCEL); only the global-machine commands
    /// (`BeginTermination`/`TerminateCall`/`Merge`/`Split`) are a
    /// [`EffectKind::CallLifecycleCommand`].
    pub fn effect_kind(&self) -> EffectKind {
        match self {
            // Leg messages — service-authored SIP toward a leg (originate / relay
            // / respond / provisional / leg create / rule-driven teardown).
            RuleAction::RelayToPeer { .. }
            | RuleAction::RelayToLeg { .. }
            | RuleAction::RelayFirstBare180 { .. }
            | RuleAction::RelayFailureToALeg { .. }
            | RuleAction::RespondToALeg { .. }
            | RuleAction::AnswerALegNewDialog { .. }
            | RuleAction::Respond { .. }
            | RuleAction::AckLeg { .. }
            | RuleAction::CreateLeg { .. }
            | RuleAction::DestroyLeg { .. }
            | RuleAction::CancelLeg { .. }
            | RuleAction::CancelPendingReinvite { .. }
            | RuleAction::TerminateLeg { .. }
            | RuleAction::SendRequestToLeg { .. }
            | RuleAction::SendProvisionalToLeg { .. }
            | RuleAction::SendPrackToLeg { .. }
            | RuleAction::SendReinvite { .. }
            | RuleAction::SendNotify { .. }
            | RuleAction::RetransmitALeg2xx => EffectKind::LegMessage,
            // Call-lifecycle commands — the one service → global hop (X3).
            RuleAction::BeginTermination { .. }
            | RuleAction::TerminateCall
            | RuleAction::Merge { .. }
            | RuleAction::Split { .. } => EffectKind::CallLifecycleCommand,
            // Guard timers.
            RuleAction::ScheduleTimer { .. }
            | RuleAction::CancelTimer { .. }
            | RuleAction::CancelAllTimers => EffectKind::GuardTimer,
            // Cursor moves — the transition itself / machine deactivation.
            RuleAction::SetState { .. } | RuleAction::ClearState { .. } => EffectKind::CursorMove,
            // Bookkeeping — local data writes / async kicks, no wire output.
            RuleAction::ConfirmDialog { .. }
            | RuleAction::UpdateLegState { .. }
            | RuleAction::AddTagMapping { .. }
            | RuleAction::AddCdrEvent { .. }
            | RuleAction::DeactivateRule { .. }
            | RuleAction::CacheSdpOnLegDialog { .. }
            | RuleAction::SetPolicyUpdateBody { .. }
            | RuleAction::SetPromotePem { .. }
            | RuleAction::SetTransfer { .. }
            | RuleAction::ReferAsyncHttp { .. }
            | RuleAction::ServiceHttpRequest { .. }
            | RuleAction::FailureAsyncHttp { .. }
            | RuleAction::ReleaseAsyncHttp { .. }
            | RuleAction::SetSubscriptions { .. }
            | RuleAction::SetReroute { .. }
            | RuleAction::SetFeatures { .. }
            | RuleAction::MergeCallExt { .. }
            | RuleAction::RecordLimiterHolds { .. }
            | RuleAction::ResolveCancelledReinvite { .. } => EffectKind::Bookkeeping,
        }
    }
}

/// The **refined view** of a [`Call`] a rule reads (ADR-0020 X8) — the entire
/// read surface a rule handler, `Match::filter` guard, or service `init` hook
/// gets. A borrowed newtype exposing only the *semantic* call state; there is
/// deliberately **no `Deref` and no raw escape hatch** to the underlying
/// `Call`, so framework internals are not part of the rule-author interface
/// and can change freely. The write side is unchanged: rules mutate only by
/// returning [`RuleAction`]s through the executor.
///
/// Hidden by design (the framework-internal fields — do not add accessors for
/// these): `topology` (the HA `(p,b)` version vector), `worker_index`,
/// `sampled`/`trace_id`/`root_span_id` (observability), `message_count`,
/// `terminating_refresh_legs`, `a_leg_pending_vias`/`a_leg_pending_cseq`
/// (relay frame state), `limiter_entries`, `timers`, `active_rules`,
/// `policy_update_headers`/`policy_update_body`, `billing_context`,
/// `emergency`. Add an accessor only when a real rule needs it — never
/// speculatively.
#[derive(Clone, Copy)]
pub struct RuleCall<'a>(&'a Call);

impl<'a> RuleCall<'a> {
    /// Wrap a call for rule consumption. Built by the engine at the dispatch
    /// boundary (router / seed path); constructing one grants only the narrow
    /// read surface below.
    pub fn new(call: &'a Call) -> Self {
        Self(call)
    }

    // ── identity / lifecycle ─────────────────────────────────────────────
    pub fn call_ref(&self) -> &'a str {
        &self.0.call_ref
    }
    pub fn state(&self) -> CallModelState {
        self.0.state
    }
    /// Every machine's current cursor (ADR-0016 X4) — read-only; `SetState` /
    /// `ClearState` are the only writers.
    pub fn sm_cursors(&self) -> &'a BTreeMap<MachineId, StateLabel> {
        &self.0.sm_cursors
    }
    pub fn created_at(&self) -> i64 {
        self.0.created_at
    }

    // ── legs ─────────────────────────────────────────────────────────────
    pub fn a_leg(&self) -> &'a Leg {
        &self.0.a_leg
    }
    pub fn b_legs(&self) -> &'a [Leg] {
        &self.0.b_legs
    }
    /// The read-only snapshot of the original a-leg INVITE (URI/headers/body).
    pub fn a_leg_invite(&self) -> &'a ALegInviteSnapshot {
        &self.0.a_leg_invite
    }

    // ── routing / decision ───────────────────────────────────────────────
    /// The opaque decision-layer token (carries the reroute plan, ADR-0017).
    pub fn callback_context(&self) -> Option<&'a str> {
        self.0.callback_context.as_deref()
    }
    pub fn features(&self) -> Option<&'a FeatureActivations> {
        self.0.features.as_ref()
    }
    pub fn active_peer(&self) -> Option<&'a ActivePeer> {
        self.0.active_peer.as_ref()
    }
    pub fn tag_map(&self) -> &'a [TagMapping] {
        &self.0.tag_map
    }
    /// The b-leg real-tag ↔ a-facing-tag mapping for a given b-leg dialog.
    pub fn find_by_b_tag(&self, b_leg_id: &str, b_tag: &str) -> Option<&'a TagMapping> {
        call::helpers::find_by_b_tag(self.0, b_leg_id, b_tag)
    }
    /// Both peered leg ids (`active_peer`), or empty when split.
    pub fn all_peered_legs(&self) -> Vec<String> {
        call::helpers::all_peered_legs(self.0)
    }

    // ── service slices (typed data backing; the cursor lives in sm_cursors) ──
    pub fn ext(&self) -> Option<&'a ExtMap> {
        self.0.ext.as_ref()
    }
    pub fn transfer_state(&self) -> Option<&'a TransferState> {
        self.0.transfer.as_ref()
    }
    pub fn transfer_active(&self) -> bool {
        call::helpers::transfer_active(self.0)
    }
    pub fn relay_first_18x_strategy(&self) -> Option<RelayFirst18xStrategy> {
        call::helpers::relay_first_18x_strategy(self.0)
    }
    pub fn relay_first_18x_first_relayed(&self) -> bool {
        call::helpers::relay_first_18x_first_relayed(self.0)
    }
    pub fn relay_first_18x_stored_a_tag(&self) -> Option<&'a str> {
        call::helpers::relay_first_18x_stored_a_tag(self.0)
    }
    /// The active `relay18x.messages` policy (defaults to `FIRST`).
    pub fn relay_first_18x_messages(&self) -> call::features::Relay18xMessages {
        call::helpers::relay_first_18x_messages(self.0)
    }
    /// Whether an 18x with this *upstream* status value was already relayed
    /// (the `ONE_PER_VALUE` dedupe test).
    pub fn relay_first_18x_value_relayed(&self, status: u16) -> bool {
        call::helpers::relay_first_18x_value_relayed(self.0, status)
    }
    pub fn promote_pem_state(&self) -> Option<&'a PromotePemState> {
        self.0.promote_pem.as_ref()
    }
    pub fn promote_pem_promoted(&self) -> bool {
        call::helpers::promote_pem_promoted(self.0)
    }
    pub fn promote_pem_window_open(&self) -> bool {
        call::helpers::promote_pem_window_open(self.0)
    }
    /// The SDP cached on a b-leg early dialog (`fake-prack`).
    pub fn cached_sdp_for_leg_dialog(&self, leg_id: &str, b_tag: &str) -> Option<&'a [u8]> {
        call::helpers::cached_sdp_for_leg_dialog(self.0, leg_id, b_tag)
    }

    // ── subscribed release events + established-call reroute (newkahneed-009) ─
    /// The release events the decision backend subscribed to on the last
    /// applied `Route` (written via `SetSubscriptions` / `apply_route`).
    pub fn subscriptions(&self) -> &'a [call::ReleaseEventKind] {
        &self.0.subscriptions
    }
    /// Whether the backend subscribed to `event` — the gate that decides
    /// consult-the-engine vs act-locally when the event fires.
    pub fn subscribed(&self, event: call::ReleaseEventKind) -> bool {
        self.0.subscriptions.contains(&event)
    }
    /// The in-flight established-call reroute slice (written via `SetReroute`).
    pub fn reroute_state(&self) -> Option<&'a call::RerouteState> {
        self.0.reroute.as_ref()
    }
    /// Whether an established-call reroute is in flight (the activation guard
    /// for the `release-reroute` rules, mirroring `transfer_active`).
    pub fn reroute_active(&self) -> bool {
        self.0.reroute.is_some()
    }

    // ── bookkeeping (read-only; written via `AddCdrEvent`) ───────────────
    pub fn cdr_events(&self) -> &'a [CdrEvent] {
        &self.0.cdr_events
    }
}

/// The resolved context a rule sees. Built by the executor/router from the
/// event + call (the source's match-narrowed `RuleContext`, flattened). The
/// call is exposed as the narrow [`RuleCall`] view — the framework keeps the
/// full `Call` on its side of the seam (ADR-0020 X8).
pub struct RuleContext<'a> {
    pub call: RuleCall<'a>,
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
    pub fn timer_type(&self) -> Option<&TimerType> {
        match self.event {
            CallEvent::Timer { timer_type, .. } => Some(timer_type),
            _ => None,
        }
    }
    /// For a fired [`TimerType::Service`] timer: `(owning service, key)`. `None`
    /// for every other event (including core timer fires). The branch point for
    /// a rule that funnels several watchdog keys through one
    /// [`Match::service_timers`] matcher.
    pub fn service_timer_key(&self) -> Option<(&MachineId, &str)> {
        match self.event {
            CallEvent::Timer { timer_type: TimerType::Service { service_id, key }, .. } => {
                Some((service_id, key))
            }
            _ => None,
        }
    }
    pub fn timeout_method(&self) -> Option<&str> {
        match self.event {
            CallEvent::Timeout { method, .. } => method.as_deref(),
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
    /// target in a **relayable** state (GAP-P8b-2)? `false` exactly when the
    /// relay would go nowhere useful: no peer leg resolves, the peer leg is
    /// `Terminated` (e.g. a failed b-leg whose `/call/failure` reroute is still
    /// pending), or the target dialog has no remote tag yet (a replacement leg
    /// still `Trying`). An `Early` peer dialog WITH a remote tag counts as
    /// relayable (RFC 3311 §5.1 early-dialog UPDATE is the normal case). Uses
    /// the SAME resolver as the executor's relay path
    /// ([`call::helpers::resolve_relay_peer`]), so match and action never
    /// disagree. Exposed on the context (usable from [`Match::filter`]) so a
    /// service rule can own failover-pending policy; the CORE default is the
    /// `update-peer-unavailable` local 491.
    pub fn peer_relay_ready(&self) -> bool {
        let to_tag = self.request().and_then(|r| r.to.tag.as_deref());
        call::helpers::relay_peer_dialog_ready(self.call.0, self.source_leg_id, to_tag)
    }

    /// The leg the event arrived on.
    pub fn source_leg(&self) -> Option<&'a Leg> {
        if self.source_leg_id == self.call.a_leg().leg_id {
            Some(self.call.a_leg())
        } else {
            self.call.b_legs().iter().find(|l| l.leg_id == self.source_leg_id)
        }
    }
    /// The dialog the event is on (confirmed dialog, else the first).
    pub fn source_dialog(&self) -> Option<&Dialog> {
        let leg = self.source_leg()?;
        call::helpers::confirmed_dialog(leg).or_else(|| leg.dialogs.first())
    }
}

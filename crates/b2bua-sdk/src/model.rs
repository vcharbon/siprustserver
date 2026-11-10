//! The rule-engine core types — port of `RuleDefinition.ts` (declarative
//! `Match`, `RuleContext`, the `RuleAction` vocabulary) reduced to a concrete
//! (non-type-narrowed) shape sufficient for the basic B2BUA. Service-layer
//! ext narrowing and rule composition are deferred with their consumers.

use call::features::FeatureActivations;
use call::{CdrEventType, ExtMap, LegKind, MachineId, StateLabel, TerminationCause, TimerKey};
use sip_message::draft::Entry;
use sip_message::header::HeaderName;

pub use crate::body::{Body, BodyAuthor};
pub use crate::effect::{Effect, EffectKind};
pub use crate::glare::GlareRefusal;
pub use crate::leg_end::LegEnd;
pub use crate::lifecycle::{AttemptOpening, LifecycleEvent, RouteAnswer};
pub use crate::matcher::{Match, MatchKind, StatusMatch};
use crate::relayed_final::RelayedFinal;
pub use crate::rule_call::RuleCall;
pub use crate::rule_context::RuleContext;
pub use crate::rule_diagnostic::RuleDiagnostic;
pub use crate::timer_delay::TimerDelay;

pub const CORE_LAYER: u8 = 0;
#[allow(dead_code)]
pub const SERVICE_LAYER: u8 = 1;

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
    /// ([`CallEvent::is_asynchronous_trigger`](crate::event::CallEvent::is_asynchronous_trigger)). Every other rule is absorbed
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
    /// ACK a leg's confirmed dialog with the answer another leg gave (RFC
    /// 3264 §4), read off that dialog's own offer/answer state: where its 2xx
    /// made an offer the ACK answers, `answer` rides the ACK, and with no
    /// answer yet the ACK waits for the relayed one that brings it; where the
    /// 2xx made none (its INVITE carried the offer, or a reliable provisional
    /// did and its PRACK answers it, RFC 3262 §5) the ACK is bare.
    AckLegAnswering {
        leg_id: String,
        answer: Option<Body>,
    },
    /// Owe the current message on `leg_id`, evidence the record cannot
    /// explain (ADR-0055 §4), what RFC 3261 requires of it, and take into the
    /// record the dialog state it proves, so the teardown that follows ends
    /// every dialog the record or the evidence names.
    EndOnEvidence {
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
    /// service arms its own watchdog under a [`TimerType::Service`](call::TimerType::Service) key.
    ///
    /// A [`TimerType::Keepalive`](call::TimerType::Keepalive) is armed exactly one `keepalive_interval_sec`
    /// out (`call::helpers::keepalive_fire_at`), a larger `delay` noted as a
    /// defect; an arm of the framework's `Rung`, `NotifyAnswerCap` or
    /// `HeldAnswer` is dropped as one. Every other type keeps its `delay`.
    ScheduleTimer {
        key: TimerKey,
        delay: TimerDelay,
    },
    /// Snooze the timer `key` (CONTEXT.md "Snooze"), meant for the one whose
    /// fire the rule handles: arm it again `delay` out, as
    /// [`Self::ScheduleTimer`] does, and count one more snooze of it, which
    /// `call::helpers::snoozes` reads until the key is armed fresh.
    SnoozeTimer {
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
    /// fire-and-forget effect carrying the request JSON. The router's callout fold
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
    /// [`CallEvent::InternalEvent::body`](crate::event::CallEvent::InternalEvent::body) verbatim (binary-safe — never a
    /// character string). The `correlation_id` is service-minted and echoed in
    /// the result `payload`, so a consuming rule's `Match::internal_event()
    /// .topic("service-http-result").filter(..)` disambiguates concurrent
    /// in-flight requests. If no port is injected the callout fold still folds an
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
    /// fire-and-forget effect carrying the request JSON. The router's callout fold
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
    /// (`callback_context`, `event`). The router's callout fold attaches the
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
    /// Snooze `key` for `delay` ([`RuleAction::SnoozeTimer`]).
    pub fn snooze(key: TimerKey, delay: TimerDelay) -> Self {
        RuleAction::SnoozeTimer { key, delay }
    }
    /// Disarm `key` ([`RuleAction::CancelTimer`]).
    pub fn cancel(key: TimerKey) -> Self {
        RuleAction::CancelTimer { key }
    }
}

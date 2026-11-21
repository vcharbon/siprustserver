//! # announcement — an out-of-tree callflow service (ADR-0016)
//!
//! An early-media **MRF announcement** service, built against the public Rule
//! SDK ([`b2bua_sdk`]) **alone** — it has no dependency on `b2bua`. It proves the
//! out-of-crate integrator seam: a separate crate can author a full per-call
//! state machine, park an unadopted media leg, broker early media, drive an
//! MSCML control channel, and hand off to the framework's normal bridge.
//!
//! ## Flow (`OfferingMrf → Announcing → [*]`)
//!
//! When the route the setup applies requests an announcement (its own
//! `service_ext["announcement"]`, which lands on `call.ext` and defers the
//! normal destination routing), the service:
//!
//! 1. **init** seeds the cursor at `OfferingMrf` and launches an unadopted
//!    `media` leg toward the MRF (in parallel with setup).
//! 2. **@OfferingMrf**, on the media leg's `200 OK`: brokers the MRF's SDP onto
//!    the caller as an unreliable `183` (early media) on an early dialog of its
//!    own, opens the MSCML control channel with an INFO `<play>`, and advances
//!    to `Announcing`.
//! 3. **@Announcing**, on the MRF's INFO MSCML `<response>` success: BYEs the
//!    media leg, dials the real destination (an adopted leg, under the attempt
//!    `init` read from the setup route: its cause, label and slices), and **deactivates**
//!    the machine (ADR-0016 X9 — the terminal `[*]`). The destination is now a
//!    normal adopted leg, so the framework's core `confirm-dialog` rule answers
//!    the caller with its SDP, under a tag of its own whose dialog the 2xx
//!    confirms (the early media's ends with it), and bridges the two — no
//!    announcement rule needed, and no dead cursor lingers on the bridged call.
//! 4. **@Announcing**, on a `<response>` **failure** (max-duration abort, no-answer,
//!    a final-announcement reject): a reject-teardown — send the caller its 4xx on
//!    the early dialog and terminate. The caller never got a 2xx, so this leans on
//!    the generic layer keeping the a-leg `Early`: the parked leg
//!    is an unadopted `Media` leg, so core `confirm-dialog` does not confirm the
//!    a-leg off its 200, and `BeginTermination` resolves the unanswered a-leg with
//!    its 4xx (no BYE) — no service-side a-leg un-confirm repair.
//!
//! A media-leg failure/timeout while offering or announcing terminates the call
//! (the one-hop service→global command, `BeginTermination`).

use b2bua_sdk::rules::{
    leg_kind, AttemptCause, AttemptOpening, Body, CdrEventType, Direction, Effect, LegEnd, LegKind,
    LegState, Match, Method, RouteAnswer, RuleAction, RuleCall, RuleContext, RuleHandleResult,
    Terminal, TerminationCause, TimeoutKind,
};
use b2bua_sdk::{define_service, sm_rule};
use serde::{Deserialize, Serialize};

pub mod mscml;

/// The ext key this service stores its data under (== the service/machine id).
pub const EXT_KEY: &str = "announcement";

/// The ext key, inside the service's slice, of the destination dial `init`
/// records ([`Dial`]).
const DIAL_KEY: &str = "dial";

/// The announcement a route requests: its `service_ext["announcement"]` slice.
#[derive(Debug, Clone, Deserialize)]
struct Request {
    /// The clip to play (opaque id / URI handed to the MRF in the MSCML `<play>`).
    clip_id: String,
    /// The media server to offer the announcement.
    mrf_host: String,
    mrf_port: u16,
    /// The real destination to dial once the clip finishes.
    dest_host: String,
    dest_port: u16,
}

/// The attempt the destination dial opens, as `init` reads it from the route
/// the setup applied: a failover's when an earlier attempt exists, dialling
/// that route's answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Dial {
    cause: AttemptCause,
    label: Option<String>,
    service_ext: std::collections::BTreeMap<String, serde_json::Value>,
}

impl Dial {
    fn at_setup(call: &RuleCall, route: &RouteAnswer) -> Self {
        let cause =
            if call.attempts().is_empty() { AttemptCause::Route } else { AttemptCause::Failover };
        Self { cause, label: route.label.clone(), service_ext: route.service_ext.clone() }
    }

    fn opening(self) -> AttemptOpening {
        AttemptOpening::new(
            self.cause,
            RouteAnswer { label: self.label, service_ext: self.service_ext },
        )
    }
}

/// The announcement service's per-call data, carried in `call.ext["announcement"]`:
/// the setup route's slice verbatim, with the destination [`Dial`] `init`
/// records under `"dial"`. Replication-safe: it is a plain JSON object on the
/// already-replicated `ext` map.
#[derive(Debug, Clone, Deserialize)]
struct AnnData {
    #[serde(flatten)]
    request: Request,
    dial: Dial,
}

fn ann_data(call: &RuleCall) -> Option<AnnData> {
    let v = call.ext()?.get(EXT_KEY)?.clone();
    serde_json::from_value(v).ok()
}

/// The announcement the route the setup applied requests, and the service's
/// data for it: the route's own slice, never one a route the limiter refused
/// left on the call, with the destination dial beside it.
fn requested(call: &RuleCall) -> Option<(Request, serde_json::Value)> {
    let route = call.setup_route()?;
    let mut slice = route.service_ext.get(EXT_KEY)?.clone();
    let request = serde_json::from_value(slice.clone()).ok()?;
    let dial = serde_json::to_value(Dial::at_setup(call, route)).ok()?;
    slice.as_object_mut()?.insert(DIAL_KEY.to_string(), dial);
    Some((request, slice))
}

/// The parked media leg toward the MRF (the single unadopted `media`-kind leg
/// that is not yet torn down).
fn media_leg_id(call: &RuleCall) -> Option<String> {
    call.b_legs()
        .iter()
        .find(|l| leg_kind(l) == LegKind::Media && l.state() != LegState::Terminated)
        .map(|l| l.leg_id.clone())
}

fn ok(actions: Vec<RuleAction>) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(actions))
}

// ── handlers ─────────────────────────────────────────────────────────────────

/// @OfferingMrf — the MRF answered the media leg. Broker its SDP onto the caller
/// as a 183 early-media, ACK the media dialog, open the MSCML control channel,
/// and advance to `Announcing`.
fn on_media_answer(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let data = ann_data(&ctx.call)?;
    let media = media_leg_id(&ctx.call)?;
    let resp = ctx.response()?;
    let mrf_sdp = resp.body().clone();
    ok(vec![
        // Establish the media dialog so the MSCML INFO can ride it.
        RuleAction::ConfirmDialog { leg_id: media.clone() },
        RuleAction::AckLeg { leg_id: media.clone(), body: None },
        // Early media: the MRF's SDP onto the caller as an unreliable 183.
        RuleAction::SendProvisionalToLeg {
            leg_id: "a".to_string(),
            status: 183,
            reason: "Session Progress".to_string(),
            body: Some(Body::from_leg(mrf_sdp.to_vec(), media.clone())),
            to_tag: None,
            p_early_media: Some("sendrecv".to_string()),
        },
        // Open the MSCML control channel: play the clip.
        RuleAction::SendRequestToLeg {
            leg_id: media,
            method: "INFO".to_string(),
            body: Some(Body::own(
                mscml::build_play(&data.request.clip_id),
                Some(mscml::CONTENT_TYPE.to_string()),
            )),
            headers: vec![],
        },
        RuleAction::SetState { machine: MACHINE, to: State::Announcing.label() },
    ])
}

/// @Announcing — the MRF reports the clip finished (MSCML `<response>` success).
/// Answer the INFO, BYE the media leg, dial the real destination under the
/// attempt the setup route states ([`Dial`]), and deactivate.
fn on_mscml_done(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let AnnData { request, dial } = ann_data(&ctx.call)?;
    let media = media_leg_id(&ctx.call)?;
    ok(vec![
        // Answer the MRF's in-dialog INFO (the B2BUA is its UAS).
        RuleAction::Respond {
            status: 200,
            reason: "OK".to_string(),
            body: vec![],
            content_type: None,
        },
        // Tear down the media leg (it is Confirmed → DestroyLeg BYEs it).
        RuleAction::DestroyLeg { leg_id: media, headers: vec![] },
        // Dial the real destination as a normal adopted leg; core `confirm-dialog`
        // will answer the caller with its SDP and bridge on its 200.
        RuleAction::CreateLeg {
            new_ruri: Some(format!("sip:{}:{}", request.dest_host, request.dest_port)),
            destination: (request.dest_host, request.dest_port),
            new_from: None,
            new_to: None,
            callback_context: None,
            body_override: None,
            header_updates: vec![],
            header_adds: vec![],
            kind: None,
            attempt: Some(dial.opening()),
        },
        // The announcement is done: hand off to the destination leg + core bridge
        // and **deactivate** the machine (ADR-0016 X9) rather than lingering at a
        // dead `Bridging` cursor — the destination is now a normal adopted leg.
        RuleAction::ClearState { machine: MACHINE },
    ])
}

/// Map an MSCML playback-failure `code` onto the SIP final the caller gets on its
/// early dialog. MSCML's failure numbering overlaps SIP for the common cases (a
/// max-duration abort surfaces as `480`); anything outside SIP's failure range
/// collapses to a `500`.
fn mscml_reject_status(code: u16) -> (u16, &'static str) {
    match code {
        480 => (480, "Temporarily Unavailable"),
        486 => (486, "Busy Here"),
        c if (400..600).contains(&c) => (c, "Announcement Rejected"),
        _ => (500, "Announcement Rejected"),
    }
}

/// @Announcing — the MRF reports the clip **failed** (MSCML `<response>` with a
/// non-2xx code: max-duration abort, no-answer, a final-announcement reject). The
/// caller only ever saw a `183` early dialog, so this is a reject-teardown: answer
/// the INFO, send the caller its 4xx final, and terminate.
///
/// This path is the whole point of announce-then-reject. The parked media leg is
/// an **unadopted** `Media` leg, so core `confirm-dialog` does NOT mark the a-leg
/// `Confirmed` off its 200 — the a-leg is still `Early`. The 4xx this rule
/// answers her with is her INVITE's final, so her leg reads `Terminated` — no
/// BYE toward the caller, and no spurious ADR-0022 503 on a later turn (e.g. a
/// crossing BYE from the media leg). No un-confirm repair is needed — the
/// generic layer keeps the a-side honest.
fn on_mscml_failed(ctx: &RuleContext) -> Option<RuleHandleResult> {
    media_leg_id(&ctx.call)?; // fire only while a parked media leg exists
    let req = ctx.request()?;
    let code = mscml::response_code(req).unwrap_or(500);
    let (status, reason) = mscml_reject_status(code);
    ok(vec![
        // Answer the MRF's in-dialog INFO (the B2BUA is its UAS).
        RuleAction::Respond {
            status: 200,
            reason: "OK".to_string(),
            body: vec![],
            content_type: None,
        },
        // The caller's INVITE is still unanswered (early media only) — send its
        // 4xx final before tearing down (begin-termination assumes the firing rule
        // already replied to a Trying/Early a-leg).
        RuleAction::RelayFailureToALeg { status, reason: reason.to_string() },
        // Terminate: the confirmed media leg is BYE'd, the Early a-leg is resolved
        // by its just-sent 4xx (no BYE). No a-leg un-confirm workaround here.
        RuleAction::BeginTermination {
            reason: Some("announcement-clip-failed".to_string()),
            cause: TerminationCause::DecisionRelease,
            by_leg: None,
        },
    ])
}

/// @Announcing — the MRF never answered the MSCML `<play>` INFO (its Timer
/// F): the announcement cannot play. The media leg carries nothing of the
/// call, so the core default would end that leg alone and leave the caller
/// ringing; the caller, who saw only a `183` early dialog, is answered 504
/// (RFC 3261 §21.5.5: the media server did not answer in time) and the call
/// terminates, the confirmed media leg BYE'd.
fn on_play_timeout(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let media = media_leg_id(&ctx.call)?;
    ok(vec![
        RuleAction::RelayFailureToALeg { status: 504, reason: "Server Time-out".to_string() },
        RuleAction::BeginTermination {
            reason: Some("announcement-mrf-timeout".to_string()),
            cause: TerminationCause::Timeout(TimeoutKind::Transaction),
            by_leg: Some(media),
        },
    ])
}

/// @OfferingMrf/@Announcing — the media leg failed (MRF rejected/timed out).
/// Terminate the call cleanly (the one-hop service → global command).
fn on_media_failure(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let media = media_leg_id(&ctx.call)?;
    let resp = ctx.response()?;
    let status = resp.status();
    let reason = resp.reason().to_string();
    ok(vec![
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Reject,
            leg_id: media.clone(),
            status_code: Some(status as i64),
            reason: Some("announcement-mrf-failure".to_string()),
        },
        // Record the media leg's final on the leg BEFORE begin-termination (the
        // `route-failure` idiom): its INVITE transaction already received a final
        // response, so it reads `Terminated` — NOT still ringing, which
        // begin-termination would CANCEL, holding the call open for a 487 that
        // will never come.
        RuleAction::TerminateLeg { leg_id: media.clone(), end: LegEnd::InviteRejected },
        // The caller's INVITE is still unanswered (early media only) — answer it
        // with the MRF's failure before tearing the call down (begin-termination
        // assumes the firing rule already replied to a Trying/Early a-leg).
        RuleAction::RelayFailureToALeg { status, reason: reason.to_string() },
        RuleAction::BeginTermination {
            reason: Some("announcement-mrf-failure".to_string()),
            cause: TerminationCause::RemoteFinal,
            by_leg: Some(media),
        },
    ])
}

/// Is the event a response on the parked media leg? (the rule filter — the media
/// leg is the unadopted `media`-kind leg).
fn on_media_leg(ctx: &RuleContext) -> bool {
    media_leg_id(&ctx.call).as_deref() == Some(ctx.source_leg_id)
}

// ── the service machine ──────────────────────────────────────────────────────

define_service! {
    id: "announcement",
    machine: MACHINE,
    states: State { OfferingMrf, Announcing },
    // Activate iff the routing decision requested an announcement; seed the
    // cursor and launch the unadopted media leg toward the MRF.
    init: |call: &RuleCall| {
        let (data, slice) = requested(call)?;
        let seed = b2bua_sdk::rules::ServiceSeed::new(State::OfferingMrf.label())
            .with_data(move |c| {
                c.ext.get_or_insert_with(Default::default).insert(EXT_KEY.to_string(), slice);
            });
        Some(
            seed.with_actions(vec![
                RuleAction::CreateLeg {
                    destination: (data.mrf_host.clone(), data.mrf_port),
                    new_ruri: Some(format!("sip:mrf@{}:{}", data.mrf_host, data.mrf_port)),
                    new_from: None,
                    new_to: None,
                    callback_context: None,
                    body_override: None,
                    header_updates: vec![],
                    header_adds: vec![],
                    kind: Some(LegKind::Media),
                    attempt: None,
                },
            ]),
        )
    },
    rules: [
        // The MRF answered the media leg → 183 early media + MSCML <play>.
        sm_rule! {
            id: "announcement-media-answer",
            machine: MACHINE,
            active: [ State::OfferingMrf ],
            transitions: [ State::OfferingMrf => State::Announcing ],
            effects: [
                Effect::Originate { method: Method::Ack, label: "ACK → media (confirm early dialog)" },
                Effect::Provisional { status: 183, label: "early media (MRF SDP) → A" },
                Effect::Originate { method: Method::Info, label: "MSCML <play> → media" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(on_media_leg),
            handle: on_media_answer,
        },
        // The MRF's MSCML <response> success → BYE media, dial destination.
        sm_rule! {
            id: "announcement-mscml-done",
            machine: MACHINE,
            active: [ State::Announcing ],
            transitions: [ State::Announcing => Terminal ],
            effects: [
                Effect::Respond { status: 200, label: "200 OK → media (answer MSCML INFO)" },
                Effect::Originate { method: Method::Bye, label: "BYE → media leg" },
                Effect::Originate { method: Method::Invite, label: "INVITE → destination" },
            ],
            matcher: Match::request()
                .method("INFO")
                .direction(Direction::FromB)
                .filter(|ctx| {
                    on_media_leg(ctx)
                        && ctx.request().is_some_and(mscml::is_success_response)
                }),
            handle: on_mscml_done,
        },
        // The MRF's MSCML <response> reports failure (max-duration/no-answer/final
        // reject) → reject-teardown: 4xx to the caller's early dialog, terminate.
        sm_rule! {
            id: "announcement-mscml-failed",
            machine: MACHINE,
            active: [ State::Announcing ],
            transitions: [ State::Announcing => Terminal ],
            effects: [
                Effect::Respond { status: 200, label: "200 OK → media (answer MSCML INFO)" },
                Effect::Relay { label: "clip-failed final → A" },
                Effect::LifecycleCommand { label: "terminate (announcement clip failed)" },
            ],
            matcher: Match::request()
                .method("INFO")
                .direction(Direction::FromB)
                .filter(|ctx| {
                    on_media_leg(ctx)
                        && ctx.request().is_some_and(mscml::is_failure_response)
                }),
            handle: on_mscml_failed,
        },
        // The MRF never answered the <play> INFO → 504 to the caller, terminate.
        sm_rule! {
            id: "announcement-play-timeout",
            machine: MACHINE,
            active: [ State::Announcing ],
            transitions: [ State::Announcing => Terminal ],
            effects: [
                Effect::Relay { label: "504 → A (the MRF never answered the play INFO)" },
                Effect::LifecycleCommand { label: "terminate (MRF timeout)" },
            ],
            matcher: Match::timeout()
                .filter(|ctx| on_media_leg(ctx) && ctx.timeout_method() == Some("INFO")),
            handle: on_play_timeout,
        },
        // The media leg failed while offering/announcing → terminate the call.
        sm_rule! {
            id: "announcement-media-failure",
            machine: MACHINE,
            active: [ State::OfferingMrf, State::Announcing ],
            transitions: [],
            effects: [
                Effect::Relay { label: "MRF failure → A" },
                Effect::LifecycleCommand { label: "terminate (MRF failure)" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .direction(Direction::FromB)
                .filter(|ctx| {
                    on_media_leg(ctx) && ctx.response().is_some_and(|r| r.status() >= 300)
                }),
            handle: on_media_failure,
        },
    ],
}

/// The service descriptor the host process registers (in `b2bua-runner`'s
/// `compose_services()` / the harness) — the only public entry point.
pub fn service() -> b2bua_sdk::rules::ServiceDef {
    service_def()
}

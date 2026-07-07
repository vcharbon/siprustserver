//! `relayFirst18xTo180` — the early-media masking callflow **service** that hides
//! forking/failover from the caller. Port of
//! `src/b2bua/rules/custom/relayFirst18xTo180.ts`, expressed as an ADR-0016
//! `define_service!` state machine (mirroring the `transfer` retrofit, slice 7).
//!
//! The machine has two states tracking how far the masking has progressed:
//!   - **`Masking`** — the first 18x has not been relayed yet. The next 18x from
//!     any b-leg is rewritten into a bare 180 (no SDP, no `100rel`) toward A and
//!     the call advances to `Suppressing`.
//!   - **`Suppressing`** — the first 18x is out (as a bare 180). Later 18x
//!     across every b-leg follow the `relay18x.messages` policy (default FIRST:
//!     all suppressed; ALL: each relayed downgraded; ONE_PER_VALUE: one per
//!     distinct upstream status value) — every relayed one reuses the first
//!     180's To-tag, and so does the 200 OK, so the caller sees one stable
//!     callee identity across forking/failover.
//!
//! Its cursor is a read-only **projection** (see [`project_cursor`], mirroring the
//! `global-call` / `transfer` projections) of two authoritative facts that already
//! live on the call: the active strategy (`features.relay_first_18x_to_180` —
//! `drop-sdp` / `keep-sdp` / `fake-prack`; `promote-pem-to-200` is owned by the
//! PEM service and never activates this machine) and whether the first 18x has
//! been relayed (`Call.relay_first_18x.first_relayed`, flipped by the
//! `RelayFirstBare180` action). The projection runs in `invariants::finalize`, so
//! the cursor is set at call setup (the router finalizes `handle_initial_invite`'s
//! result) — before the first 18x — and maintained on every event. Activation is
//! therefore implicit in the strategy: the delayed-offer fallback that nulls the
//! feature in `apply_route` (no alice SDP under `fake-prack`) deactivates the
//! machine the same turn, so the rules go inert and the call falls back to plain
//! relay. Each rule is gated by `active_states` instead of the old
//! `module_active`/`is_fake_prack` *strategy* filter; the `fake-prack`-only rules
//! keep an `is_fake_prack` filter because the machine is active for all three
//! strategies. Handlers are unchanged from the pre-retrofit core rules.
//!
//! These rules ride `default_rules()` (the flat list, like `transfer`): their
//! `machine_active` gate keeps them dormant until the cursor is projected, and
//! `pick_ranked` ranks SERVICE_LAYER above CORE so they win the events they
//! handle. `relay_first_18x_service_def()` is registered in the doc-generator
//! registry (`b2bua-runner::compose_services`) so `docs/sm/relayFirst18x.md` is
//! generated from the same declared `active_states`/`transitions`/`effects`.

use b2bua_sdk::{define_service, sm_rule};
use call::features::RelayFirst18xStrategy;
use call::{Call, CdrEventType, Direction, LegDisposition, LegState, TimerType};
use sip_message::message_helpers::{get_header, get_headers};
use sip_message::{Method, SipResponse};

use super::model::{
    Effect, Match, MessageTransform, RuleAction, RuleContext, RuleDefinition, RuleHandleResult,
};
use super::sdp_answer::{build_answer_from_offer, SdpBuildResult};

fn ok(actions: Vec<RuleAction>) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(actions))
}

/// RFC 3262: a reliable 1xx carries `Require: 100rel` and a numeric `RSeq`.
fn reliable_rseq(resp: &SipResponse) -> Option<i64> {
    let has_100rel = get_headers(&resp.headers, "require")
        .iter()
        .any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("100rel")));
    if !has_100rel {
        return None;
    }
    get_header(&resp.headers, "rseq").and_then(|r| r.trim().parse::<i64>().ok())
}

/// True iff an 18x-masking strategy this machine owns is active (`drop-sdp` /
/// `keep-sdp` / `fake-prack` — *not* `promote-pem-to-200`). The single source of
/// the cursor projection's activation gate.
fn relay_first_18x_active(call: &Call) -> bool {
    matches!(
        call::helpers::relay_first_18x_strategy(call),
        Some(RelayFirst18xStrategy::DropSdp)
            | Some(RelayFirst18xStrategy::KeepSdp)
            | Some(RelayFirst18xStrategy::FakePrack)
    )
}

/// The `fake-prack` strategy is active (a per-rule guard on the fake-prack-only
/// rules — the machine itself is active for all three masking strategies).
fn is_fake_prack(ctx: &RuleContext) -> bool {
    ctx.call.relay_first_18x_strategy() == Some(RelayFirst18xStrategy::FakePrack)
}

/// The source leg is NOT mid-CANCEL (newkahneed-024). A 2xx crossing a CANCEL
/// on the wire is a **reap** case — CORE `cancel-200-crossing` ACK+BYEs the
/// abandoned callee — regardless of which relay machine is armed. Without this
/// guard the SERVICE_LAYER 2xx rule out-ranks the CORE reap and relays/merges
/// the crossing 200 into the being-rejected a-leg, orphaning the callee in a
/// one-sided established dialog (the 012/016 symptom, resurrected by machine
/// composition).
fn source_leg_not_cancelling(ctx: &RuleContext) -> bool {
    ctx.source_leg()
        .map(|l| l.disposition != LegDisposition::Cancelling)
        .unwrap_or(true)
}

/// fake-prack AND the current UPDATE carries **no body** — the session-refresh
/// case the local-answer handler is meant for. An UPDATE that carries an SDP
/// *offer* must NOT be answered locally with a bodyless 200 (that leaves the
/// offer unanswered, RFC 3264 §5); it declines here and falls through to CORE
/// `relay-update`, which forwards it to the b-leg early dialog and returns the
/// callee's real answer (an early-dialog UPDATE is the RFC 3311 §5.1 normal
/// case; the b-leg dialog carries its callee tag, so the relay is well-formed).
fn is_fake_prack_bodyless_update(ctx: &RuleContext) -> bool {
    is_fake_prack(ctx) && ctx.request().map(|r| r.body.is_empty()).unwrap_or(false)
}

// The `relayFirst18x` callflow service (ADR-0016). `Phase` is the declared
// machine; its cursor is a projection of `(strategy, first_relayed)` (see
// `project_cursor`), so the `transitions` a rule declares are diagram edges, never
// enforced (the cursor is moved by the projection in `finalize`, not by a
// `SetState`). `init` stays dormant (`None`): the projection sets the cursor at
// setup from the strategy `apply_route` already decided, so no seed is needed.
define_service! {
    id: "relayFirst18x",
    machine: RELAY_FIRST_18X_MACHINE,
    states: Phase { Masking, Suppressing },
    init: |_call| None,
    rules: [
        // ── suppress-18x — first 18x → bare 180; police the rest ──────────────
        // Wins over CORE `relay-provisional` by SERVICE_LAYER. On the first 18x:
        // relay a bare 180 (minting the a-facing tag + seeding the tag map, in the
        // `RelayFirstBare180` executor) and advance to `Suppressing`. Later 18x
        // follow the `relay18x.messages` policy (Routing API `Relay18x.messages`):
        // FIRST (default) suppresses them all; ALL relays each one (downgraded to
        // a bare 180 under the SAME stored a-facing tag — the mask stays one
        // early dialog); ONE_PER_VALUE relays the first 18x of each distinct
        // *upstream* status value and suppresses repeats. Reliable 1xx is PRACKed
        // by the B2BUA itself (alice never saw it); `fake-prack` caches bob's SDP
        // per `(leg, To-tag)` dialog — strictly, one cache per fork (GAP-P7-1).
        sm_rule! {
            id: "suppress-18x",
            machine: RELAY_FIRST_18X_MACHINE,
            active: [ Phase::Masking, Phase::Suppressing ],
            transitions: [ Phase::Masking => Phase::Suppressing ],
            effects: [
                Effect::Provisional { status: 180, label: "first 18x → bare 180 → A (drop SDP/100rel)" },
                Effect::Originate { method: Method::Prack, label: "PRACK → B (B2BUA absorbs reliable 1xx)" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(1)
                .direction(Direction::FromB),
            handle: |ctx| {
                let resp = ctx.response()?;
                let b_tag = resp.to.tag.clone().unwrap_or_default();
                let rseq = reliable_rseq(resp);
                let invite_cseq = resp.cseq.seq as i64;
                let leg = ctx.source_leg_id.to_string();
                let fake_prack = is_fake_prack(ctx);

                // Reliable 1xx → the B2BUA PRACKs the b-leg itself (alice never
                // sees the reliable provisional, so she won't PRACK).
                let prack_action = rseq.map(|rseq| RuleAction::SendPrackToLeg {
                    leg_id: leg.clone(),
                    rseq,
                    invite_cseq,
                    b_tag: b_tag.clone(),
                });
                // fake-prack: cache bob's SDP per dialog when 100rel is in play.
                let cache_action = if fake_prack && rseq.is_some() && !resp.body.is_empty() {
                    Some(RuleAction::CacheSdpOnLegDialog {
                        leg_id: leg.clone(),
                        b_tag: b_tag.clone(),
                        body: resp.body.clone(),
                    })
                } else {
                    None
                };

                if ctx.call.relay_first_18x_first_relayed() {
                    // Subsequent 18x — the `relay18x.messages` policy decides
                    // whether it is relayed again (downgraded, under the stored
                    // a-facing tag) or suppressed. Either way it is PRACKed +
                    // cached (per-fork dialog state is policy-independent).
                    let relay_again = match ctx.call.relay_first_18x_messages() {
                        call::features::Relay18xMessages::All => true,
                        call::features::Relay18xMessages::First => false,
                        call::features::Relay18xMessages::OnePerValue => {
                            !ctx.call.relay_first_18x_value_relayed(resp.status)
                        }
                    };
                    if !relay_again {
                        let mut actions = Vec::new();
                        if let Some(a) = prack_action {
                            actions.push(a);
                        }
                        if let Some(a) = cache_action {
                            actions.push(a);
                        }
                        actions.push(RuleAction::AddCdrEvent {
                            event_type: CdrEventType::Provisional,
                            leg_id: leg,
                            status_code: Some(resp.status as i64),
                            reason: None,
                        });
                        return ok(actions);
                    }
                    // Relay-again falls through to the RelayFirstBare180 branch:
                    // its executor reuses the stored a-facing tag (one stable
                    // early dialog toward the caller) and records the upstream
                    // status value for ONE_PER_VALUE dedupe.
                }

                // First 18x (or a later one the messages policy relays again) —
                // mint/reuse the a-facing tag + relay as a bare 180 (the
                // executor owns the IdGen and the tag-map seeding, and flips
                // `first_relayed`, which the projection mirrors → `Suppressing`).
                let mut actions = vec![
                    RuleAction::RelayFirstBare180 {
                        leg_id: leg.clone(),
                        b_tag: b_tag.clone(),
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Provisional,
                        leg_id: leg.clone(),
                        status_code: Some(resp.status as i64),
                        reason: None,
                    },
                ];
                if let Some(a) = prack_action {
                    actions.push(a);
                }
                // Cache MUST run after relay-to-peer — the relay creates the
                // early dialog the cache keys on (by b_tag).
                if let Some(a) = cache_action {
                    actions.push(a);
                }
                ok(actions)
            },
        },
        // ── force-tag-consistency — reuse the stored To-tag on 200 OK ────────
        //
        // Composes with CORE `confirm-dialog`: pre-seed the tag map with the
        // stored a-facing tag so the 200 OK To-tag matches the first 180, and
        // (fake-prack) stage the winning dialog's cached SDP into the relayed 200.
        // The engine is first-match-wins, so when this rule acts it must replay
        // `confirm-dialog`'s action sequence itself (see `confirm_dialog_actions`);
        // when it has nothing to pre-seed it declines (`None`) and `confirm-dialog`
        // (CORE, ranked just below) handles the 2xx. No cursor move — the call
        // bridges via the `global-call` machine; the masking property persists.
        // A 2xx on a leg being CANCELled is NOT matched (`source_leg_not_cancelling`,
        // newkahneed-024): it defers to CORE `cancel-200-crossing`, which reaps
        // the abandoned callee (ACK+BYE) instead of bridging it to a caller the
        // teardown is already rejecting.
        sm_rule! {
            id: "force-tag-consistency",
            machine: RELAY_FIRST_18X_MACHINE,
            active: [ Phase::Masking, Phase::Suppressing ],
            transitions: [],
            effects: [
                Effect::Relay { label: "200 OK → A (reuse stored To-tag; fake-prack: inject cached SDP)" },
                Effect::LifecycleCommand { label: "merge A↔B (bridge)" },
                Effect::GuardTimer { timer: TimerType::NoAnswer, label: "cancel B no-answer" },
                Effect::GuardTimer { timer: TimerType::GlobalDuration, label: "arm max-duration" },
                Effect::GuardTimer { timer: TimerType::Keepalive, label: "arm keepalive" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .filter(source_leg_not_cancelling),
            handle: |ctx| {
                let resp = ctx.response()?;
                let b_tag = resp.to.tag.clone().unwrap_or_default();
                let leg = ctx.source_leg_id.to_string();
                let mut actions = Vec::new();

                if let Some(stored) = ctx.call.relay_first_18x_stored_a_tag() {
                    actions.push(RuleAction::AddTagMapping {
                        a_tag: stored.to_string(),
                        b_leg_id: leg.clone(),
                        b_tag: b_tag.clone(),
                    });
                }

                if is_fake_prack(ctx) {
                    let cached = ctx.call.cached_sdp_for_leg_dialog(&leg, &b_tag)
                        .map(|b| b.to_vec());
                    match cached {
                        Some(b) if !b.is_empty() => {
                            actions.push(RuleAction::SetPolicyUpdateBody { body: b });
                        }
                        _ if resp.body.is_empty() => {
                            // No cache AND bob's 200 has no body — surface a CDR
                            // marker (alice's call may break: no SDP at confirm).
                            actions.push(RuleAction::AddCdrEvent {
                                event_type: CdrEventType::Provisional,
                                leg_id: leg.clone(),
                                status_code: Some(200),
                                reason: Some("fake-prack:200-ok-no-sdp".to_string()),
                            });
                        }
                        _ => {} // bob repeated SDP in 200 → relay as-is.
                    }
                }

                if actions.is_empty() {
                    // Decline so confirm-dialog handles the 2xx by itself.
                    return None;
                }
                // Re-emit confirm-dialog's actions after our pre-seeding so the
                // 2xx is actually relayed (this rule wins the match; it composes
                // with confirm-dialog by replaying its action sequence).
                actions.extend(confirm_dialog_actions(ctx));
                ok(actions)
            },
        },
        // ── absorb-prack-200 — swallow bob's 200 for the B2BUA's own PRACK ───
        // Wins over CORE `relay-non-invite-200`. The B2BUA originated the PRACK
        // (alice never saw the reliable 1xx), so its 200 must not reach alice.
        sm_rule! {
            id: "absorb-prack-200",
            machine: RELAY_FIRST_18X_MACHINE,
            active: [ Phase::Masking, Phase::Suppressing ],
            transitions: [],
            effects: [],
            matcher: Match::response()
                .method("PRACK")
                .status_class(2)
                .direction(Direction::FromB),
            handle: |ctx| {
                ok(vec![RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Provisional,
                    leg_id: ctx.source_leg_id.to_string(),
                    status_code: Some(200),
                    reason: None,
                }])
            },
        },
        // ── fake-prack: locally answer b-leg UPDATE (wins over relay-update) ─
        // alice has no committed bob-SDP to negotiate against, so the B2BUA
        // answers bob's early-dialog UPDATE itself: a skeleton-fit answer derived
        // from alice's INVITE offer (488 on no codec intersection), advancing the
        // cached SDP to bob's UPDATE offer.
        sm_rule! {
            id: "fake-prack-handle-update-from-b",
            machine: RELAY_FIRST_18X_MACHINE,
            active: [ Phase::Masking, Phase::Suppressing ],
            transitions: [],
            effects: [
                Effect::Respond { status: 200, label: "200 OK → B (local skeleton-fit answer)" },
                Effect::Respond { status: 488, label: "488 → B (no codec intersection)" },
            ],
            matcher: Match::request()
                .method("UPDATE")
                .direction(Direction::FromB)
                .filter(is_fake_prack),
            handle: |ctx| {
                let req = ctx.request()?;
                let b_tag = req.from.tag.clone().unwrap_or_default();
                let leg = ctx.source_leg_id.to_string();

                if req.body.is_empty() {
                    return ok(vec![RuleAction::Respond {
                        status: 200,
                        reason: "OK".to_string(),
                        body: vec![],
                        content_type: None,
                    }]);
                }

                let alice_body = &ctx.call.a_leg_invite().body;
                match build_answer_from_offer(&req.body, alice_body, &ctx.config.sip_local_ip, ctx.now_ms) {
                    SdpBuildResult::Ok(body) => ok(vec![
                        RuleAction::Respond {
                            status: 200,
                            reason: "OK".to_string(),
                            body,
                            content_type: Some("application/sdp".to_string()),
                        },
                        RuleAction::CacheSdpOnLegDialog {
                            leg_id: leg,
                            b_tag,
                            body: req.body.clone(),
                        },
                    ]),
                    _ => ok(vec![RuleAction::Respond {
                        status: 488,
                        reason: "Not Acceptable Here".to_string(),
                        body: vec![],
                        content_type: None,
                    }]),
                }
            },
        },
        // ── fake-prack: locally answer a-leg early-dialog **bodyless** UPDATE ─
        // A no-body UPDATE (session-timer / dialog refresh, RFC 4028) carries no
        // offer to negotiate, so answer 200 OK locally — do NOT wake the b-leg.
        // An UPDATE that carries an SDP *offer* is deliberately NOT matched here
        // (`is_fake_prack_bodyless_update`): answering it with a bodyless 200
        // would strand alice's offer (RFC 3264 §5). It falls through to CORE
        // `relay-update`, which forwards the offer to the b-leg early dialog and
        // relays the callee's real answer back — the RFC 3311 §5.1 normal case.
        // (early state only; after merge, normal in-dialog UPDATE relay applies.)
        sm_rule! {
            id: "fake-prack-handle-update-from-a",
            machine: RELAY_FIRST_18X_MACHINE,
            active: [ Phase::Masking, Phase::Suppressing ],
            transitions: [],
            effects: [
                Effect::Respond { status: 200, label: "200 OK → A (local answer; bodyless refresh)" },
            ],
            matcher: Match::request()
                .method("UPDATE")
                .direction(Direction::FromA)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(is_fake_prack_bodyless_update),
            handle: |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 200,
                    reason: "OK".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        },
    ],
}

/// The machine-gated service rules, kept under the pre-retrofit name for
/// `default_rules()` (the engine runs them via the flat rule list; the
/// `define_service!`-generated `rules()` is the source). Mirrors `transfer_rules`.
pub fn relay_first_18x_rules() -> Vec<RuleDefinition> {
    rules()
}

/// The `relayFirst18x` service descriptor — registered in the doc-generator
/// registry (`b2bua-runner::compose_services`) so `docs/sm/relayFirst18x.md` is
/// generated from the same declared `active_states`/`transitions`/`effects` the
/// engine gates on. (Its `init` is dormant, so it is *not* in the runtime
/// `services` list — the rules ride `default_rules()`; this is doc-only, like
/// `transfer_service_def`.)
pub fn relay_first_18x_service_def() -> crate::rules::ServiceDef {
    service_def()
}

/// Project the authoritative `(strategy, first_relayed)` into the `relayFirst18x`
/// machine cursor (ADR-0016), mirroring the `global-call` / `transfer`
/// projections. Called from `invariants::finalize`, so the cursor is set at call
/// setup (the router finalizes `handle_initial_invite`'s result, after
/// `apply_route` decided the strategy) — before the first 18x — and removed when
/// the strategy is absent / nulled (the delayed-offer self-disable), deactivating
/// the machine so the masking rules go inert.
pub fn project_cursor(call: &mut Call) {
    if relay_first_18x_active(call) {
        let label = if call::helpers::relay_first_18x_first_relayed(call) {
            Phase::Suppressing.label()
        } else {
            Phase::Masking.label()
        };
        call.sm_cursors.insert(RELAY_FIRST_18X_MACHINE, label);
    } else {
        call.sm_cursors.remove(&RELAY_FIRST_18X_MACHINE);
    }
}

/// Replay `confirm-dialog`'s action sequence (the `force-tag-consistency` rule
/// composes with it: it wins the 2xx match, so it must emit confirm-dialog's
/// effects itself). Kept in sync with `defaults.rs::confirm-dialog`.
fn confirm_dialog_actions(ctx: &RuleContext) -> Vec<RuleAction> {
    let b = ctx.source_leg_id.to_string();
    let a = ctx.call.a_leg().leg_id.clone();
    let max_duration = ctx
        .call
        .features()
        .map(|f| f.platform.max_duration_sec)
        .unwrap_or(3600);
    // Operator/worker knob (`B2buaConfig::keepalive_interval_sec`, production
    // default 300 s) — not the per-call feature; see `defaults::keepalive_interval`.
    let keepalive = ctx.config.keepalive_interval_sec;
    vec![
        RuleAction::ConfirmDialog { leg_id: b.clone() },
        RuleAction::Merge {
            leg_a: a,
            leg_b: b.clone(),
        },
        RuleAction::RelayToPeer {
            transform: MessageTransform::default(),
        },
        RuleAction::CancelTimer {
            id: format!("NoAnswer:{b}"),
        },
        RuleAction::CancelTimer {
            id: format!("{:?}", TimerType::SetupTimeout),
        },
        RuleAction::ScheduleTimer {
            timer_type: TimerType::GlobalDuration,
            delay_sec: max_duration,
            leg_id: None,
        },
        RuleAction::ScheduleTimer {
            timer_type: TimerType::Keepalive,
            delay_sec: keepalive,
            leg_id: None,
        },
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Answer,
            leg_id: b,
            status_code: Some(200),
            reason: None,
        },
    ]
}

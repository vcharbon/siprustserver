//! `release-reroute` — the subscribed release-event fold + the
//! **established-call reroute** treatment (newkahneed-009).
//!
//! When a subscribed internal release event fires (max-call-duration first),
//! the `max-duration` rule seeds a `ReleaseAsyncHttp`; the router consults
//! `decision.call_release` (deadline-bounded) and folds the outcome back as a
//! `call-release-result` internal event handled here:
//!
//!   - `release` → exactly today's local teardown (CDR `max_duration` +
//!     `BeginTermination`) — also the fail-safe for engine error / timeout /
//!     limiter reject on the reroute route.
//!   - `reroute` → apply the `Route`-shaped decision to the CONNECTED call:
//!     create the replacement b-leg (INVITE toward the new destination,
//!     carrying A's INVITE-snapshot offer unless the route overrides the
//!     body), on its 2xx ACK it and **re-INVITE the a-leg** with the new
//!     leg's answer SDP (the bridge — same one-way-audio discipline as the
//!     REFER machine's a-realign, `refer_transfer.rs`), and on A's 2xx
//!     `Merge(a, new)` + **BYE the old b-leg**. The whole treatment is
//!     bounded by one overall guard timer (`release_reroute_guard_sec`).
//!
//! **Failure policy (documented choice):** any failure of the treatment — the
//! replacement leg rejects / never answers, A rejects the realign re-INVITE,
//! or the guard expires — tears the WHOLE call down locally (reason
//! `release-reroute-failed`/`-timeout`). The release event already declared
//! the call over; the reroute was the alternative treatment, and when it
//! fails the call must fall back to the release default rather than live past
//! its cap (the alternative — keeping the original A↔B up — would let a
//! misbehaving announcement target extend calls indefinitely). This is
//! simpler than consulting `call_failure` per the setup-failover path and is
//! the documented divergence.
//!
//! State rides the typed `Call.reroute` slice (mirrors `Call.transfer`):
//! presence is the activation guard, so every rule here is inert on ordinary
//! calls. These are CORE rules registered BEFORE the generic core set
//! (`defaults::default_rules_with`), so within CORE they out-rank
//! `confirm-dialog` / `relay-provisional` / `route-failure` by order; the
//! explicit `overrides` document the displacement. In-dialog INVITEs from
//! either original party are 491'd while the reroute is in flight (RFC 5407
//! §3.1 retry-later, the same treatment the transfer machine applies); a BYE
//! from either party still rides `relay-bye` and ends the call.

use call::{
    ByeDisposition, CdrEventType, Direction, LegDisposition, LegState, MachineId, ReroutePhase,
    RerouteState, TimerType,
};

use super::model::{Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, CORE_LAYER};

/// Owner id for the reroute's service-owned guard timer (`Service:release-reroute:guard`).
pub const RELEASE_REROUTE_MACHINE: MachineId = MachineId::new("release-reroute");
const GUARD_KEY: &str = "guard";

fn guard_timer() -> TimerType {
    TimerType::service(RELEASE_REROUTE_MACHINE, GUARD_KEY)
}

fn ok(actions: Vec<RuleAction>) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(actions))
}

fn rule(
    id: &'static str,
    overrides: &'static [&'static str],
    matcher: Match,
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
) -> RuleDefinition {
    RuleDefinition::core(id, CORE_LAYER, overrides, matcher, handle)
}

/// The event's source leg is the reroute's replacement b-leg.
fn is_new_leg(ctx: &RuleContext) -> bool {
    ctx.call.reroute_state().map(|r| r.new_leg_id.as_str()) == Some(ctx.source_leg_id)
}

/// Local teardown with the reroute-failure reason: force the call down like
/// the `release` outcome would, recording why. `BeginTermination` BYEs every
/// confirmed leg (A + old B + a confirmed new leg) and CANCELs a still-ringing
/// new leg; the `→ terminated` invariant settles obligations exactly once.
fn fail_teardown(ctx: &RuleContext, reason: &'static str) -> Vec<RuleAction> {
    vec![
        RuleAction::cancel_timer(&guard_timer(), None),
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Bye,
            leg_id: ctx.call.a_leg().leg_id.clone(),
            status_code: None,
            reason: Some("max_duration".into()),
        },
        RuleAction::SetReroute { state: None },
        RuleAction::BeginTermination { reason: Some(reason.into()) },
    ]
}

/// The release/reroute rule set (CORE; registered before the generic core
/// rules in `defaults::default_rules_with`).
pub fn release_reroute_rules() -> Vec<RuleDefinition> {
    vec![
        // ── release consult folded: `release` (or fail-safe) → local teardown.
        // Exactly the body the unsubscribed `max-duration` rule emits, so the
        // wire behaviour with-and-without a subscription is identical when the
        // backend says Release.
        rule(
            "release-result-release",
            &[],
            Match::internal_event().topic("call-release-result").outcome("release"),
            |ctx| {
                ok(vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Bye,
                        leg_id: ctx.call.a_leg().leg_id.clone(),
                        status_code: None,
                        reason: Some("max_duration".into()),
                    },
                    RuleAction::BeginTermination { reason: Some("max-duration".into()) },
                ])
            },
        ),
        // ── release consult folded: `reroute` → replacement b-leg + slice.
        // Output parity with the initial/failover route (newkahneed-005): the
        // shared `route_fold_parity_actions` applies features (incl. the
        // GlobalDuration re-arm — the rerouted call gets the route's fresh
        // cap), service_ext, subscriptions, and the router-admitted limiter
        // holds; `CreateLeg` honors the identity/header/body rewrites.
        rule(
            "release-result-reroute",
            &[],
            Match::internal_event().topic("call-release-result").outcome("reroute"),
            |ctx| {
                let payload = match ctx.event {
                    crate::event::CallEvent::InternalEvent { payload, .. } => payload,
                    _ => return None,
                };
                let fold = super::defaults::parse_route_fold(payload)?;
                // Same recipe as the CreateLeg executor, so the slice's
                // `new_leg_id` names the leg CreateLeg is about to mint.
                let new_leg_id = format!("b-{}", ctx.call.b_legs().len() + 1);
                let old_leg_id = ctx.call.active_peer().map(|p| p.leg_b.clone());
                let no_answer = fold
                    .no_answer
                    .or(fold.features.as_ref().and_then(|f| f.no_answer_timeout_sec));

                let mut actions = super::defaults::route_fold_parity_actions(&fold, ctx);
                actions.push(RuleAction::CreateLeg {
                    destination: fold.destination,
                    new_ruri: fold.new_ruri,
                    new_from: fold.new_from,
                    new_to: fold.new_to,
                    no_answer_timeout_sec: no_answer,
                    callback_context: fold.callback_context,
                    body_override: fold.body_override,
                    header_updates: fold.header_updates,
                    kind: None,
                });
                actions.push(RuleAction::SetReroute {
                    state: Some(RerouteState {
                        phase: ReroutePhase::BLegDialing,
                        new_leg_id,
                        old_leg_id,
                        started_at_ms: ctx.now_ms,
                    }),
                });
                // One overall guard bounding the whole treatment (dial +
                // realign + old-leg BYE) — a wedged reroute must never extend
                // the call past its cap.
                actions.push(RuleAction::ScheduleTimer {
                    timer_type: guard_timer(),
                    delay_sec: ctx.config.release_reroute_guard_sec,
                    leg_id: None,
                });
                ok(actions)
            },
        ),
        // ── replacement leg progress: absorb its 18x (A is established — a
        // provisional must NOT be relayed onto her answered dialog).
        rule(
            "reroute-b-provisional",
            &["relay-provisional"],
            Match::response()
                .method("INVITE")
                .status_class(1)
                .direction(Direction::FromB)
                .filter(is_new_leg),
            |ctx| {
                ok(vec![RuleAction::UpdateLegState {
                    leg_id: ctx.source_leg_id.to_string(),
                    state: LegState::Early,
                    disposition: None,
                }])
            },
        ),
        // ── replacement leg answers → ACK it, bridge A onto its SDP.
        // Displaces `confirm-dialog` (which would Merge + relay the 2xx to A's
        // long-answered INVITE). The re-INVITE toward A carries the NEW leg's
        // answer SDP — the same "offer the active answer" discipline as the
        // REFER a-realign (refer_transfer.rs one-way-audio note).
        rule(
            "reroute-b-200",
            &["confirm-dialog", "relay-reinvite-response"],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(is_new_leg),
            |ctx| {
                let resp = ctx.response()?;
                let st = ctx.call.reroute_state()?.clone();
                let new_leg = st.new_leg_id.clone();
                let mut new_state = st;
                new_state.phase = ReroutePhase::ARealigning;
                ok(vec![
                    RuleAction::UpdateLegState {
                        leg_id: new_leg.clone(),
                        state: LegState::Confirmed,
                        disposition: Some(LegDisposition::Bridged),
                    },
                    RuleAction::ConfirmDialog { leg_id: new_leg.clone() },
                    RuleAction::AckLeg { leg_id: new_leg.clone(), body: Vec::new(), content_type: None },
                    RuleAction::cancel_timer(&TimerType::NoAnswer, Some(&new_leg)),
                    RuleAction::SendReinvite {
                        leg_id: "a".to_string(),
                        body: resp.body.clone(),
                        add_headers: vec![],
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Answer,
                        leg_id: new_leg,
                        status_code: Some(200),
                        reason: Some("release-reroute".into()),
                    },
                    RuleAction::SetReroute { state: Some(new_state) },
                ])
            },
        ),
        // ── replacement leg fails (3xx–6xx) → the documented fail-teardown.
        rule(
            "reroute-b-fail",
            &["route-failure"],
            Match::response()
                .method("INVITE")
                .direction(Direction::FromB)
                .filter(|ctx| {
                    ctx.response().map(|r| r.status >= 300).unwrap_or(false) && is_new_leg(ctx)
                }),
            |ctx| {
                let resp = ctx.response()?;
                let new_leg = ctx.source_leg_id.to_string();
                let mut actions = vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Reject,
                        leg_id: new_leg.clone(),
                        status_code: Some(resp.status as i64),
                        reason: Some("release-reroute-failed".into()),
                    },
                    RuleAction::TerminateLeg {
                        leg_id: new_leg.clone(),
                        bye_disposition: Some(ByeDisposition::Rejected),
                    },
                    RuleAction::cancel_timer(&TimerType::NoAnswer, Some(&new_leg)),
                ];
                actions.extend(fail_teardown(ctx, "release-reroute-failed"));
                ok(actions)
            },
        ),
        // ── replacement leg never answers (its route-armed NoAnswer fires) →
        // the fail-teardown, displacing the CORE `no-answer` rule whose
        // `/call/failure` consult belongs to the setup path.
        rule(
            "reroute-b-no-answer",
            &["no-answer"],
            Match::timer().timer_type(TimerType::NoAnswer).filter(|ctx| {
                let timer_leg = match ctx.event {
                    crate::event::CallEvent::Timer { leg_id, .. } => leg_id.as_deref(),
                    _ => None,
                };
                ctx.call.reroute_state().map(|r| r.new_leg_id.as_str()).is_some()
                    && ctx.call.reroute_state().map(|r| r.new_leg_id.as_str()) == timer_leg
            }),
            |ctx| {
                let st = ctx.call.reroute_state()?.clone();
                let mut actions = vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Timeout,
                        leg_id: st.new_leg_id.clone(),
                        status_code: None,
                        reason: Some("release-reroute-failed".into()),
                    },
                    RuleAction::DestroyLeg { leg_id: st.new_leg_id.clone() },
                ];
                actions.extend(fail_teardown(ctx, "release-reroute-failed"));
                ok(actions)
            },
        ),
        // ── A answers the realign re-INVITE → bridge complete: ACK A,
        // merge(a, new), BYE the old b-leg, clear the slice. The call
        // continues A↔new under the route's re-armed cap; a later hangup
        // rides the ordinary `relay-bye`.
        rule(
            "reroute-a-realign-200",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromA)
                .filter(|ctx| {
                    ctx.source_leg_id == "a"
                        && ctx.call.reroute_state().map(|r| r.phase)
                            == Some(ReroutePhase::ARealigning)
                }),
            |ctx| {
                let st = ctx.call.reroute_state()?.clone();
                let mut actions = vec![
                    RuleAction::AckLeg { leg_id: "a".to_string(), body: Vec::new(), content_type: None },
                    RuleAction::cancel_timer(&guard_timer(), None),
                    RuleAction::Merge { leg_a: "a".to_string(), leg_b: st.new_leg_id.clone() },
                ];
                if let Some(old) = st.old_leg_id.clone() {
                    // BYE the displaced b-leg (DestroyLeg: confirmed → BYE +
                    // `bye_sent`; its 200 resolves via `resolve-bye-response`).
                    actions.push(RuleAction::DestroyLeg { leg_id: old });
                }
                actions.push(RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Answer,
                    leg_id: "a".to_string(),
                    status_code: Some(200),
                    reason: Some("release-reroute-completed".into()),
                });
                actions.push(RuleAction::SetReroute { state: None });
                ok(actions)
            },
        ),
        // ── A rejects the realign re-INVITE → the fail-teardown (the txn
        // layer already ACKed the non-2xx final).
        rule(
            "reroute-a-realign-fail",
            &[],
            Match::response()
                .method("INVITE")
                .direction(Direction::FromA)
                .filter(|ctx| {
                    ctx.response().map(|r| r.status >= 300).unwrap_or(false)
                        && ctx.source_leg_id == "a"
                        && ctx.call.reroute_state().map(|r| r.phase)
                            == Some(ReroutePhase::ARealigning)
                }),
            |ctx| {
                let resp = ctx.response()?;
                let mut actions = vec![RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Reject,
                    leg_id: "a".to_string(),
                    status_code: Some(resp.status as i64),
                    reason: Some("release-reroute-failed".into()),
                }];
                actions.extend(fail_teardown(ctx, "release-reroute-failed"));
                ok(actions)
            },
        ),
        // ── overall guard expired mid-treatment → the fail-teardown.
        rule(
            "reroute-guard-timeout",
            &[],
            Match::timer()
                .timer_type(TimerType::service(RELEASE_REROUTE_MACHINE, GUARD_KEY))
                .filter(|ctx| ctx.call.reroute_active()),
            |ctx| {
                let mut actions = vec![RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: ctx.call.a_leg().leg_id.clone(),
                    status_code: None,
                    reason: Some("release-reroute-timeout".into()),
                }];
                actions.extend(fail_teardown(ctx, "release-reroute-timeout"));
                ok(actions)
            },
        ),
        // ── glare: any in-dialog INVITE from either original party while the
        // reroute is in flight → 491 Request Pending (RFC 5407 §3.1 — retry
        // once the reroute settles). Same treatment the transfer machine's
        // realign phases apply. A BYE is deliberately NOT intercepted — it
        // rides `relay-bye` and ends the whole call.
        rule(
            "reroute-glare-reinvite",
            &["reinvite-glare", "relay-reinvite"],
            Match::request().method("INVITE").filter(|ctx| ctx.call.reroute_active()),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 491,
                    reason: "Request Pending".into(),
                    body: vec![],
                    content_type: None,
                }])
            },
        ),
    ]
}

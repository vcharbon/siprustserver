//! The basic-B2BUA default rule set (CORE_LAYER) — port of the load-bearing
//! rules in `src/b2bua/rules/defaults/`. Covers the bridged-call lifecycle:
//! INVITE → 18x → 200 → ACK → in-dialog → BYE, plus CANCEL, b-leg failure, and
//! the housekeeping timers. The 18x-management strategies, PEM/fake-prack, and
//! REFER transfer (SERVICE_LAYER) are deferred (see MIGRATION_STATUS / ADR-0010).
//!
//! Rules are registered in priority order: corner cases + failure resolution
//! first (narrow matches), broad relays last. `overrides` removes a displaced
//! rule regardless of order.

use call::{ByeDisposition, CdrEventType, Direction, CallModelState, LegDisposition, LegState, TimerType};

use super::model::{CORE_LAYER, Match, MessageTransform, RuleAction, RuleContext, RuleDefinition, RuleHandleResult};

fn rule(
    id: &'static str,
    overrides: &'static [&'static str],
    matcher: Match,
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
) -> RuleDefinition {
    RuleDefinition {
        id,
        layer: CORE_LAYER,
        overrides,
        matcher,
        handle,
    }
}

fn ok(actions: Vec<RuleAction>) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(actions))
}

fn no_transform() -> MessageTransform {
    MessageTransform::default()
}

fn keepalive_interval(ctx: &RuleContext) -> i64 {
    // The in-dialog OPTIONS keepalive interval is an operator/worker knob
    // (`B2buaConfig::keepalive_interval_sec`, production default 300 s,
    // `B2BUA_KEEPALIVE_SEC` override), not a per-call feature: a 30 s poke breaks
    // long-hold endurance traffic. The per-call `features` keepalive interval is
    // retained for compatibility but no longer drives the runtime timer.
    ctx.config.keepalive_interval_sec
}
fn max_duration(ctx: &RuleContext) -> i64 {
    ctx.call.features.as_ref().map(|f| f.platform.max_duration_sec).unwrap_or(3600)
}

/// The ordered basic-B2BUA rule list. The SERVICE_LAYER `relayFirst18xTo180`
/// rules are appended at the end; they are dormant unless a call activates the
/// feature (their column+filter gate keeps them out of `pick_ranked` otherwise),
/// and `pick_ranked` ranks SERVICE_LAYER above CORE so they win when active.
pub fn default_rules() -> Vec<RuleDefinition> {
    // The REFER seed rules are CORE_LAYER and must out-rank the generic
    // `relay-non-invite` REFER relay; registration order (earlier wins within a
    // layer) puts them first. Their match columns + `no_transfer_active` filter
    // keep them inert for non-REFER traffic.
    let mut rules = super::refer_transfer::transfer_seed_rules();
    rules.extend(core_rules());
    rules.extend(super::relay_first_18x::relay_first_18x_rules());
    rules.extend(super::promote_pem::promote_pem_rules());
    rules.extend(super::refer_transfer::transfer_rules());
    rules
}

/// The CORE_LAYER rule set.
fn core_rules() -> Vec<RuleDefinition> {
    vec![
        // ── corner cases ────────────────────────────────────────────────────
        rule(
            "cancel-200-crossing",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .leg_disposition(LegDisposition::Cancelling)
                .direction(Direction::FromB),
            |ctx| {
                let b = ctx.source_leg_id.to_string();
                ok(vec![
                    RuleAction::ConfirmDialog { leg_id: b.clone() },
                    RuleAction::AckLeg { leg_id: b.clone() },
                    RuleAction::DestroyLeg { leg_id: b },
                ])
            },
        ),
        rule(
            "resolve-cancel-response",
            &["route-failure", "absorb-stale-failure"],
            Match::response()
                .method("INVITE")
                .leg_disposition(LegDisposition::Cancelling)
                .direction(Direction::FromB)
                .filter(|ctx| ctx.response().map(|r| r.status >= 300).unwrap_or(false)),
            |ctx| {
                let b = ctx.source_leg_id.to_string();
                ok(vec![RuleAction::TerminateLeg {
                    leg_id: b,
                    bye_disposition: Some(ByeDisposition::Cancelled),
                }])
            },
        ),
        rule(
            "absorb-stale-failure",
            &[],
            Match::response()
                .method("INVITE")
                .leg_states(&[LegState::Terminated])
                .direction(Direction::FromB)
                .filter(|ctx| ctx.response().map(|r| r.status >= 300).unwrap_or(false)),
            |_ctx| ok(vec![]),
        ),
        // Re-INVITE glare (RFC 3261 §14.1 / §3.1 of RFC 5407): an INVITE arrives
        // on a dialog that already carries an in-flight inbound INVITE (a
        // re-INVITE we relayed onto this dialog and have not yet seen a final
        // response for) → reject the newcomer 491 Request Pending. More specific
        // than `relay-reinvite` (no filter), so it wins on glare. Port of
        // `reinviteGlareRule`.
        rule(
            "reinvite-glare",
            &["relay-reinvite"],
            Match::request().method("INVITE").filter(|ctx| {
                ctx.source_dialog()
                    .map(|d| d.ext.inbound_pending_requests.iter().any(|p| p.method.eq_ignore_ascii_case("INVITE")))
                    .unwrap_or(false)
            }),
            |_ctx| ok(vec![RuleAction::Respond { status: 491, reason: "Request Pending".into(), body: vec![], content_type: None }]),
        ),
        // Relay a re-INVITE response (1xx/2xx/3xx+) back to the originator. The
        // source dialog carries a pending-relay snapshot for the response CSeq
        // (captured when the re-INVITE was relayed onto this dialog) — so the
        // relay path rebuilds the response from that snapshot and removes the
        // entry on the final response. Outranks `relay-provisional`,
        // `confirm-dialog` and `route-failure`, which would otherwise claim an
        // INVITE response. Port of `relayReinviteResponseRule`.
        rule(
            "relay-reinvite-response",
            &["relay-provisional", "confirm-dialog", "route-failure"],
            Match::response().method("INVITE").filter(|ctx| {
                let cseq = match ctx.response() {
                    Some(r) => r.cseq.seq as i64,
                    None => return false,
                };
                ctx.source_dialog()
                    .map(|d| call::helpers::find_pending_request(d, cseq).is_some())
                    .unwrap_or(false)
            }),
            |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
        ),
        // ── dialog ──────────────────────────────────────────────────────────
        rule(
            "relay-provisional",
            &[],
            Match::response().method("INVITE").status_class(1).direction(Direction::FromB),
            |ctx| {
                let b = ctx.source_leg_id.to_string();
                let status = ctx.response().map(|r| r.status as i64);
                ok(vec![
                    RuleAction::UpdateLegState {
                        leg_id: b.clone(),
                        state: LegState::Early,
                        disposition: None,
                    },
                    RuleAction::RelayToPeer { transform: no_transform() },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Provisional,
                        leg_id: b,
                        status_code: status,
                        reason: None,
                    },
                ])
            },
        ),
        rule(
            "confirm-dialog",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .leg_states(&[LegState::Trying, LegState::Early])
                .direction(Direction::FromB),
            |ctx| {
                let b = ctx.source_leg_id.to_string();
                let a = ctx.call.a_leg.leg_id.clone();
                ok(vec![
                    RuleAction::ConfirmDialog { leg_id: b.clone() },
                    RuleAction::Merge { leg_a: a, leg_b: b.clone() },
                    RuleAction::RelayToPeer { transform: no_transform() },
                    RuleAction::CancelTimer { id: format!("NoAnswer:{b}") },
                    RuleAction::ScheduleTimer {
                        timer_type: TimerType::GlobalDuration,
                        delay_sec: max_duration(ctx),
                        leg_id: None,
                    },
                    RuleAction::ScheduleTimer {
                        timer_type: TimerType::Keepalive,
                        delay_sec: keepalive_interval(ctx),
                        leg_id: None,
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Answer,
                        leg_id: b,
                        status_code: Some(200),
                        reason: None,
                    },
                ])
            },
        ),
        rule(
            "relay-non-invite-200",
            &[],
            Match::response()
                .methods(&["OPTIONS", "INFO", "PRACK", "UPDATE", "REFER", "MESSAGE", "SUBSCRIBE"])
                .status_class(2),
            |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
        ),
        // ── failure ─────────────────────────────────────────────────────────
        rule(
            "route-failure",
            &[],
            Match::response()
                .method("INVITE")
                .direction(Direction::FromB)
                .filter(|ctx| ctx.response().map(|r| r.status >= 300).unwrap_or(false)),
            |ctx| {
                let b = ctx.source_leg_id.to_string();
                let (status, reason) = ctx
                    .response()
                    .map(|r| (r.status as i64, r.reason.clone()))
                    .unwrap_or((500, "Server Error".into()));
                // Tear the failed leg down + record the reject. The relay/terminate
                // (or failover) is decided next.
                let mut actions = vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Reject,
                        leg_id: b.clone(),
                        status_code: Some(status),
                        reason: Some(reason.clone()),
                    },
                    RuleAction::TerminateLeg {
                        leg_id: b.clone(),
                        bye_disposition: Some(ByeDisposition::Rejected),
                    },
                ];
                match &ctx.call.callback_context {
                    // Failover-capable call → ask /call/failure (origin external).
                    // The result (call-failure-result internal event) drives either
                    // `failover-create-leg` or `failover-terminate`. We deliberately
                    // do NOT relay or terminate here: on a reject the caller must not
                    // see the failure until the backend declines to fail over.
                    Some(cbctx) => actions.push(RuleAction::FailureAsyncHttp {
                        request: serde_json::json!({
                            "callback_context": cbctx,
                            "origin": "external",
                            "sip_code": status,
                            "sip_reason": reason,
                            "failed_leg_id": b,
                        }),
                    }),
                    // No callback context → relay the failure to the caller and
                    // tear the whole call down (the pre-failover behaviour).
                    None => {
                        actions.push(RuleAction::RelayToPeer { transform: no_transform() });
                        actions.push(RuleAction::TerminateCall);
                    }
                }
                ok(actions)
            },
        ),
        // ── failover resolution (/call/failure result) ──────────────────────
        // The async /call/failure round-trip folds its decision back via a
        // `call-failure-result` internal event. `failover` → cancel the failed
        // leg's no-answer timer + create a fresh b-leg toward the new
        // destination (A's INVITE snapshot; the relay_first_18x slice survives so
        // the new leg's To-tag stays the first 180's). Port of FailureRules.ts
        // route-failure / no-answer-failover failover branches.
        rule(
            "failover-create-leg",
            &[],
            Match::internal_event()
                .topic("call-failure-result")
                .outcome("failover"),
            |ctx| {
                let payload = match ctx.event {
                    crate::event::CallEvent::InternalEvent { payload, .. } => payload,
                    _ => return None,
                };
                let host = payload
                    .get("destination")
                    .and_then(|d| d.get("host"))
                    .and_then(|v| v.as_str())?
                    .to_string();
                let port = payload
                    .get("destination")
                    .and_then(|d| d.get("port"))
                    .and_then(|v| v.as_u64())
                    .map(|p| p as u16)
                    .unwrap_or(5060);
                let new_ruri = payload.get("new_ruri").and_then(|v| v.as_str()).map(str::to_string);
                let no_answer = payload.get("no_answer_timeout_sec").and_then(|v| v.as_i64());
                let callback_context = payload.get("callback_context").and_then(|v| v.as_str()).map(str::to_string);
                let header_updates: Vec<(String, Option<String>)> = payload
                    .get("update_headers")
                    .and_then(|v| v.as_object())
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string))).collect())
                    .unwrap_or_default();
                let failed_leg_id = payload.get("failed_leg_id").and_then(|v| v.as_str()).unwrap_or("");

                let mut actions = Vec::new();
                // Cancel the failed leg's no-answer timer (a reject can beat it;
                // for the no-answer trigger the timer already fired — harmless).
                if !failed_leg_id.is_empty() {
                    actions.push(RuleAction::CancelTimer { id: format!("NoAnswer:{failed_leg_id}") });
                }
                actions.push(RuleAction::CreateLeg {
                    destination: (host, port),
                    new_ruri,
                    no_answer_timeout_sec: no_answer,
                    callback_context,
                    // `fromInvite: "snapshot"` — keep A's INVITE body (delayed
                    // offer ⇒ none); failover is not a held-SDP transfer.
                    body_override: None,
                    header_updates,
                });
                ok(actions)
            },
        ),
        // `terminate` (or backend error) → relay the original failure to the
        // caller (response path; the no-answer path carries no status) and tear
        // the call down.
        rule(
            "failover-terminate",
            &[],
            Match::internal_event()
                .topic("call-failure-result")
                .outcome("terminate"),
            |ctx| {
                let payload = match ctx.event {
                    crate::event::CallEvent::InternalEvent { payload, .. } => payload,
                    _ => return None,
                };
                let mut actions = Vec::new();
                if let Some(status) = payload.get("status").and_then(|v| v.as_u64()) {
                    let reason = payload
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Server Internal Error")
                        .to_string();
                    actions.push(RuleAction::RelayFailureToALeg { status: status as u16, reason });
                }
                actions.push(RuleAction::BeginTermination { reason: Some("failover-declined".into()) });
                ok(actions)
            },
        ),
        rule(
            "handle-481",
            &[],
            Match::response().status_code(481).call_state(CallModelState::Active),
            |ctx| {
                let src = ctx.source_leg_id.to_string();
                ok(vec![
                    RuleAction::TerminateLeg { leg_id: src.clone(), bye_disposition: Some(ByeDisposition::ByeTimeout) },
                    RuleAction::AddCdrEvent { event_type: CdrEventType::Bye, leg_id: src, status_code: Some(481), reason: Some("Call/Transaction Does Not Exist".into()) },
                    RuleAction::BeginTermination { reason: Some("481".into()) },
                ])
            },
        ),
        // ── absorption ──────────────────────────────────────────────────────
        rule(
            "absorb-bye-200",
            &[],
            Match::response().methods(&["BYE", "CANCEL"]).status_class(2),
            |_ctx| ok(vec![]),
        ),
        rule(
            "absorb-options-200",
            &["relay-non-invite-200"],
            // Only a B2BUA-originated keepalive OPTIONS is absorbed: it leaves no
            // pending-relay snapshot on the source dialog. A relayed end-to-end
            // OPTIONS does leave one (matching the response CSeq) → this declines
            // and `relay-non-invite-200` forwards the 200 to the peer. Port of
            // `absorbOptions200Rule`'s filter.
            Match::response().method("OPTIONS").status_class(2).filter(|ctx| {
                let cseq = ctx.response().map(|r| r.cseq.seq as i64);
                match (ctx.source_dialog(), cseq) {
                    (Some(d), Some(seq)) => call::helpers::find_pending_request(d, seq).is_none(),
                    _ => true,
                }
            }),
            |ctx| {
                let leg = ctx.source_leg_id.to_string();
                ok(vec![RuleAction::CancelTimer { id: format!("KeepaliveTimeout:{leg}") }])
            },
        ),
        rule(
            "absorb-notify-200",
            &[],
            Match::response().method("NOTIFY").status_class(2),
            |_ctx| ok(vec![]),
        ),
        // ── terminating ─────────────────────────────────────────────────────
        rule(
            "resolve-bye-response",
            &["absorb-bye-200"],
            Match::response()
                .method("BYE")
                .filter(|ctx| {
                    ctx.source_leg()
                        .and_then(|l| l.bye_disposition)
                        .map(|d| d == ByeDisposition::ByeSent)
                        .unwrap_or(false)
                }),
            |ctx| {
                ok(vec![RuleAction::TerminateLeg {
                    leg_id: ctx.source_leg_id.to_string(),
                    bye_disposition: Some(ByeDisposition::ByeConfirmed),
                }])
            },
        ),
        rule(
            "resolve-cross-bye",
            &[],
            Match::request().method("BYE").call_state(CallModelState::Terminating),
            |ctx| {
                ok(vec![
                    RuleAction::Respond { status: 200, reason: "OK".into(), body: vec![], content_type: None },
                    RuleAction::TerminateLeg {
                        leg_id: ctx.source_leg_id.to_string(),
                        bye_disposition: Some(ByeDisposition::ByeReceived),
                    },
                ])
            },
        ),
        // ── relay (broad) ───────────────────────────────────────────────────
        rule("relay-ack", &[], Match::request().method("ACK"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        rule("relay-bye", &[], Match::request().method("BYE").call_state(CallModelState::Active), |ctx| {
            // Pre-mark the BYE-sending leg `bye_received` (RFC 3261 §15.1.2) so the
            // subsequent begin-termination skips it (no duplicate BYE back to the
            // sender) and only tears down the peer. Port of `relayByeRule`.
            ok(vec![
                RuleAction::Respond { status: 200, reason: "OK".into(), body: vec![], content_type: None },
                RuleAction::TerminateLeg { leg_id: ctx.source_leg_id.to_string(), bye_disposition: Some(ByeDisposition::ByeReceived) },
                RuleAction::AddCdrEvent { event_type: CdrEventType::Bye, leg_id: ctx.source_leg_id.to_string(), status_code: None, reason: None },
                RuleAction::BeginTermination { reason: Some("BYE".into()) },
            ])
        }),
        rule("relay-reinvite", &[], Match::request().method("INVITE"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        rule("relay-prack", &[], Match::request().method("PRACK"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        rule("relay-options", &[], Match::request().method("OPTIONS"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        rule("relay-info", &[], Match::request().method("INFO"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        rule("relay-update", &[], Match::request().method("UPDATE"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        rule("relay-message", &[], Match::request().method("MESSAGE"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        // ── lifecycle ───────────────────────────────────────────────────────
        rule("handle-cancel", &[], Match::cancelled(), |ctx| {
            let mut actions = Vec::new();
            for b in &ctx.call.b_legs {
                match b.state {
                    LegState::Confirmed => actions.push(RuleAction::DestroyLeg { leg_id: b.leg_id.clone() }),
                    LegState::Trying | LegState::Early => actions.push(RuleAction::CancelLeg { leg_id: b.leg_id.clone() }),
                    LegState::Terminated => {}
                }
            }
            actions.push(RuleAction::AddCdrEvent {
                event_type: CdrEventType::Cancel,
                leg_id: ctx.call.a_leg.leg_id.clone(),
                status_code: None,
                reason: None,
            });
            actions.push(RuleAction::BeginTermination { reason: Some("CANCEL".into()) });
            ok(actions)
        }),
        rule("handle-timeout", &[], Match::timeout(), |_| {
            ok(vec![RuleAction::BeginTermination { reason: Some("timeout".into()) }])
        }),
        // ── timers ──────────────────────────────────────────────────────────
        rule("no-answer", &[], Match::timer().timer_type(TimerType::NoAnswer), |ctx| {
            let leg = ctx.source_leg_id.to_string();
            let mut actions = vec![
                RuleAction::AddCdrEvent { event_type: CdrEventType::Timeout, leg_id: leg.clone(), status_code: None, reason: Some("no_answer_timeout".into()) },
                RuleAction::DestroyLeg { leg_id: leg.clone() },
            ];
            match &ctx.call.callback_context {
                // Failover-capable → ask /call/failure (origin no_answer_timeout);
                // the result drives `failover-create-leg` / `failover-terminate`.
                Some(cbctx) => actions.push(RuleAction::FailureAsyncHttp {
                    request: serde_json::json!({
                        "callback_context": cbctx,
                        "origin": "no_answer_timeout",
                        "failed_leg_id": leg,
                    }),
                }),
                None => actions.push(RuleAction::BeginTermination { reason: Some("no-answer".into()) }),
            }
            ok(actions)
        }),
        rule("max-duration", &[], Match::timer().timer_type(TimerType::GlobalDuration), |ctx| {
            ok(vec![
                RuleAction::AddCdrEvent { event_type: CdrEventType::Bye, leg_id: ctx.call.a_leg.leg_id.clone(), status_code: None, reason: Some("max_duration".into()) },
                RuleAction::BeginTermination { reason: Some("max-duration".into()) },
            ])
        }),
        rule("keepalive", &[], Match::timer().timer_type(TimerType::Keepalive).call_state(CallModelState::Active), |ctx| {
            let mut actions = Vec::new();
            for leg_id in call::helpers::all_peered_legs(ctx.call) {
                actions.push(RuleAction::SendRequestToLeg { leg_id: leg_id.clone(), method: "OPTIONS".into() });
                actions.push(RuleAction::ScheduleTimer { timer_type: TimerType::KeepaliveTimeout, delay_sec: 5, leg_id: Some(leg_id) });
            }
            actions.push(RuleAction::ScheduleTimer { timer_type: TimerType::Keepalive, delay_sec: keepalive_interval(ctx), leg_id: None });
            ok(actions)
        }),
        rule("keepalive-timeout", &[], Match::timer().timer_type(TimerType::KeepaliveTimeout).call_state(CallModelState::Active), |ctx| {
            ok(vec![
                RuleAction::TerminateLeg { leg_id: ctx.source_leg_id.to_string(), bye_disposition: Some(ByeDisposition::ByeTimeout) },
                RuleAction::AddCdrEvent { event_type: CdrEventType::Bye, leg_id: ctx.source_leg_id.to_string(), status_code: None, reason: Some("keepalive timeout".into()) },
                RuleAction::BeginTermination { reason: Some("keepalive-timeout".into()) },
            ])
        }),
        rule("terminating-safety-timeout", &[], Match::timer().timer_type(TimerType::TerminatingTimeout).call_state(CallModelState::Terminating), |ctx| {
            // A BYE we sent went unanswered within TERMINATING_TIMEOUT_MS (a lost
            // BYE, a dead UAC/UAS, or proxy churn during teardown). The call is
            // wedged in Terminating with a non-terminal `ByeSent` leg, so
            // `is_fully_resolved` never passes, `RemoveCall` is never emitted, and
            // the call — its `active_calls` slot AND its memory — leaks forever
            // (observed: active_calls pinned flat for hours after all traffic
            // stopped). Force every still-unresolved leg terminal (mirroring the
            // `is_fully_resolved` predicate) so the invariant promotes
            // Terminating→Terminated→RemoveCall and the call is reaped + the
            // replication delete propagates. If the call already resolved, the
            // loop yields no actions and this stays the harmless canary it was.
            let mut actions = Vec::new();
            for leg in std::iter::once(&ctx.call.a_leg).chain(ctx.call.b_legs.iter()) {
                let resolved = match leg.bye_disposition {
                    None => leg.state == LegState::Trying,
                    Some(b) => b.is_terminal(),
                };
                if !resolved {
                    actions.push(RuleAction::TerminateLeg {
                        leg_id: leg.leg_id.clone(),
                        bye_disposition: Some(ByeDisposition::ByeTimeout),
                    });
                }
            }
            ok(actions)
        }),
    ]
}

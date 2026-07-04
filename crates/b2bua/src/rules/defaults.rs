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
    RuleDefinition::core(id, CORE_LAYER, overrides, matcher, handle)
}

fn ok(actions: Vec<RuleAction>) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(actions))
}

fn no_transform() -> MessageTransform {
    MessageTransform::default()
}

/// Parse a `call-failure-result` payload's `update_headers` object into the
/// `(name, set-or-remove)` pairs the response/leg builders consume.
fn parse_header_updates(payload: &serde_json::Value) -> Vec<(String, Option<String>)> {
    payload
        .get("update_headers")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string))).collect())
        .unwrap_or_default()
}

fn keepalive_interval(ctx: &RuleContext) -> i64 {
    // The in-dialog OPTIONS keepalive interval is an operator/worker knob
    // (`B2buaConfig::keepalive_interval_sec`, production default 300 s,
    // `B2BUA_KEEPALIVE_SEC` override), not a per-call feature: a 30 s poke breaks
    // long-hold endurance traffic. The per-call `features` keepalive interval is
    // retained for compatibility but no longer drives the runtime timer.
    ctx.config.keepalive_interval_sec
}
fn keepalive_timeout(ctx: &RuleContext) -> i64 {
    // Grace for the in-dialog OPTIONS 200 before the leg is declared dead and the
    // call is torn down. Operator knob (`B2BUA_KEEPALIVE_TIMEOUT_SEC`, default
    // 32 s) — a hard-coded 5 s was too tight across a reboot, BYE-ing healthy
    // reclaimed dialogs whose keepalive round-trip was still settling.
    ctx.config.keepalive_timeout_sec
}
fn max_duration(ctx: &RuleContext) -> i64 {
    ctx.call.features().map(|f| f.platform.max_duration_sec).unwrap_or(3600)
}
fn ack_timeout(ctx: &RuleContext) -> i64 {
    // RFC 3261 §13.3.1.4 — the a-leg 2xx-without-ACK give-up window (operator knob
    // `B2BUA_ACK_TIMEOUT_SEC`, default 32 s = 64·T1). `<= 0` disables the watchdog.
    ctx.config.ack_timeout_sec
}
/// First a-leg 2xx-retransmit interval (RFC 3261 T1 = 500 ms). The
/// [`TimerType::AckRetransmit`](call::TimerType::AckRetransmit) timer re-arms at a
/// fixed cadence (a faithful simplification of T1→T2 doubling — it retransmits no
/// less often than RFC requires); [`TimerType::AckTimeout`](call::TimerType::AckTimeout)
/// bounds the whole window. Seconds for the `ScheduleTimer` delay_sec contract is
/// integer, so the cadence is kept as a whole second (1 s) to stay on the
/// existing seconds-granularity timer plumbing without a finer-grained API.
const ACK_RETRANSMIT_SEC: i64 = 1;

/// Shared body of the reaper-verdict rules (ADR-0020 X1): force every
/// still-unresolved leg terminal (mirroring `is_fully_resolved`, like
/// `terminating-safety-timeout`), record the reason on the CDR, and command
/// termination. No wire messages: the legs were force-resolved above, so
/// `BeginTermination` skips them all and just moves the lifecycle — finalize
/// promotes, the invariant discharges the obligations.
fn reap_force_terminal(ctx: &RuleContext, reason: &'static str) -> Option<RuleHandleResult> {
    let mut actions = Vec::new();
    for leg in std::iter::once(ctx.call.a_leg()).chain(ctx.call.b_legs().iter()) {
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
    actions.push(RuleAction::AddCdrEvent {
        event_type: CdrEventType::Bye,
        leg_id: ctx.call.a_leg().leg_id.clone(),
        status_code: None,
        reason: Some(reason.into()),
    });
    actions.push(RuleAction::BeginTermination { reason: Some(reason.into()) });
    ok(actions)
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
                let a = ctx.call.a_leg().leg_id.clone();
                let mut actions = vec![
                    RuleAction::ConfirmDialog { leg_id: b.clone() },
                    RuleAction::Merge { leg_a: a, leg_b: b.clone() },
                    RuleAction::RelayToPeer { transform: no_transform() },
                    RuleAction::CancelTimer { id: format!("NoAnswer:{b}") },
                    RuleAction::CancelTimer { id: format!("{:?}", TimerType::SetupTimeout) },
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
                ];
                // RFC 3261 §13.3.1.4: arm the a-leg 2xx-without-ACK watchdog. The
                // a-leg INVITE *server* txn went `Completed` on this final, so the
                // txn layer will NOT retransmit the 2xx proactively and at Timer H
                // deletes the un-ACKed txn silently — without this an answered call
                // whose caller never ACKs leaks until the 1 h GlobalDuration cap.
                // `AckRetransmit` re-sends the stored 2xx each cadence; `AckTimeout`
                // bounds the window and, on expiry, BYEs both legs. Both are
                // cancelled by `relay-ack` when the a-leg ACK arrives.
                if ack_timeout(ctx) > 0 {
                    actions.push(RuleAction::ScheduleTimer {
                        timer_type: TimerType::AckRetransmit,
                        delay_sec: ACK_RETRANSMIT_SEC,
                        leg_id: None,
                    });
                    actions.push(RuleAction::ScheduleTimer {
                        timer_type: TimerType::AckTimeout,
                        delay_sec: ack_timeout(ctx),
                        leg_id: None,
                    });
                }
                ok(actions)
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
                match ctx.call.callback_context() {
                    // Failover-capable call → ask /call/failure (origin external).
                    // The result (call-failure-result internal event) drives either
                    // `failover-create-leg` or `failover-terminate`. We deliberately
                    // do NOT relay or terminate here: on a reject the caller must not
                    // see the failure until the backend declines to fail over.
                    Some(cbctx) => {
                        // Event-scoped context only this site has: the failed
                        // final's non-structural headers, verbatim and in wire
                        // order (`Reason:`/`Warning:`/`X-*` for the decision
                        // backend). Structural fields already travel as typed
                        // request fields.
                        let sip_headers: Vec<serde_json::Value> = ctx
                            .response()
                            .map(|r| {
                                r.headers
                                    .iter()
                                    .filter(|h| {
                                        !crate::initial_invite::STANDARD_HEADERS
                                            .contains(&h.name.to_ascii_lowercase().as_str())
                                    })
                                    .map(|h| serde_json::json!([h.name, h.value]))
                                    .collect()
                            })
                            .unwrap_or_default();
                        actions.push(RuleAction::FailureAsyncHttp {
                            request: serde_json::json!({
                                "callback_context": cbctx,
                                "origin": "external",
                                "sip_code": status,
                                "sip_reason": reason,
                                "failed_leg_id": b,
                                "sip_headers": sip_headers,
                            }),
                        });
                    }
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
                let new_from = payload.get("new_from").and_then(|v| v.as_str()).map(str::to_string);
                let new_to = payload.get("new_to").and_then(|v| v.as_str()).map(str::to_string);
                let no_answer = payload.get("no_answer_timeout_sec").and_then(|v| v.as_i64());
                let callback_context = payload.get("callback_context").and_then(|v| v.as_str()).map(str::to_string);
                let header_updates: Vec<(String, Option<String>)> = payload
                    .get("update_headers")
                    .and_then(|v| v.as_object())
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string))).collect())
                    .unwrap_or_default();
                let failed_leg_id = payload.get("failed_leg_id").and_then(|v| v.as_str()).unwrap_or("");

                // ── failover/initial-route parity ────────────────────────────
                // The same decision response must be honored the same way on
                // both paths, so the reroute applies what `apply_route` applies:
                // features (incl. the GlobalDuration re-arm), service_ext,
                // update_body, and the limiter holds the router's fold already
                // admitted against the new target.
                let features: Option<call::features::FeatureActivations> = payload
                    .get("features")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let service_ext: call::ExtMap = payload
                    .get("service_ext")
                    .and_then(|v| v.as_object())
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                // `update_body` mirrors the decision wire shape: absent = keep
                // A's INVITE body, null = drop, string = substitute.
                let body_override = match payload.get("update_body") {
                    None => None,
                    Some(serde_json::Value::Null) => Some(Vec::new()),
                    Some(serde_json::Value::String(s)) => Some(s.clone().into_bytes()),
                    Some(_) => None,
                };
                let limiter_holds = payload
                    .get("call_limiter")
                    .and_then(|v| v.as_object())
                    .and_then(|o| {
                        let window = o.get("window")?.as_i64()?;
                        let entries: Vec<(String, i64)> = o
                            .get("entries")?
                            .as_array()?
                            .iter()
                            .filter_map(|e| {
                                Some((e.get("id")?.as_str()?.to_string(), e.get("limit")?.as_i64()?))
                            })
                            .collect();
                        Some((entries, window))
                    });

                let mut actions = Vec::new();
                // Cancel the failed leg's no-answer timer (a reject can beat it;
                // for the no-answer trigger the timer already fired — harmless).
                if !failed_leg_id.is_empty() {
                    actions.push(RuleAction::CancelTimer { id: format!("NoAnswer:{failed_leg_id}") });
                }
                let no_answer = no_answer.or(features.as_ref().and_then(|f| f.no_answer_timeout_sec));
                if let Some(f) = features {
                    // Re-arm the duration cap from the reroute's features, as the
                    // initial path does at route time (ScheduleTimer id-dedups).
                    actions.push(RuleAction::ScheduleTimer {
                        timer_type: TimerType::GlobalDuration,
                        delay_sec: f.platform.max_duration_sec,
                        leg_id: None,
                    });
                    actions.push(RuleAction::SetFeatures { features: f });
                }
                if !service_ext.is_empty() {
                    actions.push(RuleAction::MergeCallExt { ext: service_ext });
                }
                if let Some((entries, window)) = limiter_holds {
                    actions.push(RuleAction::RecordLimiterHolds { entries, window });
                    actions.push(RuleAction::ScheduleTimer {
                        timer_type: TimerType::LimiterRefresh,
                        delay_sec: ctx.config.limiter_refresh_sec,
                        leg_id: None,
                    });
                }
                actions.push(RuleAction::CreateLeg {
                    destination: (host, port),
                    new_ruri,
                    new_from,
                    new_to,
                    no_answer_timeout_sec: no_answer,
                    callback_context,
                    body_override,
                    header_updates,
                    kind: None,
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
        // `reject` → the plan declined to fail over and authored its own final
        // failure (code/reason/headers, e.g. a `Reason:` header). Send it to A and
        // tear the call down. Port-parallel of `failover-terminate` (ADR-0017).
        rule(
            "failover-reject",
            &[],
            Match::internal_event()
                .topic("call-failure-result")
                .outcome("reject"),
            |ctx| {
                let payload = match ctx.event {
                    crate::event::CallEvent::InternalEvent { payload, .. } => payload,
                    _ => return None,
                };
                let status = payload.get("code").and_then(|v| v.as_u64()).unwrap_or(500) as u16;
                let reason = payload
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Declined")
                    .to_string();
                let header_updates = parse_header_updates(payload);
                ok(vec![
                    RuleAction::RespondToALeg { status, reason, header_updates, contacts: vec![] },
                    RuleAction::BeginTermination { reason: Some("failover-reject".into()) },
                ])
            },
        ),
        // `redirect` → the plan authored a 3xx with a Contact list. Send it to A
        // (the caller retries the targets) and tear the call down (ADR-0017).
        rule(
            "failover-redirect",
            &[],
            Match::internal_event()
                .topic("call-failure-result")
                .outcome("redirect"),
            |ctx| {
                let payload = match ctx.event {
                    crate::event::CallEvent::InternalEvent { payload, .. } => payload,
                    _ => return None,
                };
                let status = payload.get("code").and_then(|v| v.as_u64()).unwrap_or(302) as u16;
                let reason = payload
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Moved Temporarily")
                    .to_string();
                let header_updates = parse_header_updates(payload);
                let contacts: Vec<(String, Option<f32>)> = payload
                    .get("contacts")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|c| {
                                let uri = c.get("uri")?.as_str()?.to_string();
                                let q = c.get("q").and_then(|v| v.as_f64()).map(|q| q as f32);
                                Some((uri, q))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                ok(vec![
                    RuleAction::RespondToALeg { status, reason, header_updates, contacts },
                    RuleAction::BeginTermination { reason: Some("failover-redirect".into()) },
                ])
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
        rule("relay-ack", &[], Match::request().method("ACK"), |ctx| {
            let mut actions = vec![RuleAction::RelayToPeer { transform: no_transform() }];
            // The a-leg ACK (Direction::FromA) arrived → the caller confirmed the
            // 2xx, so cancel the RFC 3261 §13.3.1.4 un-ACKed-2xx watchdog
            // (retransmit cadence + give-up). A b-leg ACK (FromB) leaves them
            // untouched — they only ever guard the a-leg dialog. Cancelling a timer
            // that was never armed (ack_timeout disabled, or this is a b-leg ACK)
            // is a harmless no-op in the driver.
            if ctx.direction == Direction::FromA {
                actions.push(RuleAction::CancelTimer { id: format!("{:?}", TimerType::AckRetransmit) });
                actions.push(RuleAction::CancelTimer { id: format!("{:?}", TimerType::AckTimeout) });
            }
            ok(actions)
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
            for b in ctx.call.b_legs() {
                match b.state {
                    LegState::Confirmed => actions.push(RuleAction::DestroyLeg { leg_id: b.leg_id.clone() }),
                    LegState::Trying | LegState::Early => actions.push(RuleAction::CancelLeg { leg_id: b.leg_id.clone() }),
                    LegState::Terminated => {}
                }
            }
            actions.push(RuleAction::AddCdrEvent {
                event_type: CdrEventType::Cancel,
                leg_id: ctx.call.a_leg().leg_id.clone(),
                status_code: None,
                reason: None,
            });
            actions.push(RuleAction::BeginTermination { reason: Some("CANCEL".into()) });
            ok(actions)
        }),
        rule("handle-timeout", &[], Match::timeout(), |ctx| {
            // A **pending b-leg INVITE** transaction timeout (Timer B / the long
            // INVITE backstop) on a failover-capable call is a failure the
            // decision backend must get a shot at — the dead-gateway reroute is
            // the classic failover case, and before this it was the one b-leg
            // failure that never consulted `/call/failure`. Mirror the
            // `no-answer` shape: record the timeout, destroy the failed leg
            // (CANCELs a still-early dialog; a total blackhole gets a harmless
            // raw CANCEL), and let `call-failure-result` drive the outcome.
            // Everything else (a-leg, confirmed-leg re-INVITE, BYE/OPTIONS
            // timeouts, no callback context) keeps the unconditional
            // termination below.
            let timed_out_invite = ctx
                .timeout_method()
                .map(|m| m.eq_ignore_ascii_case("INVITE"))
                .unwrap_or(false);
            let pending_b_leg = ctx.source_leg().is_some_and(|l| {
                l.leg_id != ctx.call.a_leg().leg_id
                    && matches!(l.state, LegState::Trying | LegState::Early)
            });
            if timed_out_invite && pending_b_leg {
                if let Some(cbctx) = ctx.call.callback_context() {
                    let leg = ctx.source_leg_id.to_string();
                    return ok(vec![
                        RuleAction::AddCdrEvent {
                            event_type: CdrEventType::Timeout,
                            leg_id: leg.clone(),
                            status_code: None,
                            reason: Some("transaction_timeout".into()),
                        },
                        RuleAction::DestroyLeg { leg_id: leg.clone() },
                        RuleAction::FailureAsyncHttp {
                            request: serde_json::json!({
                                "callback_context": cbctx,
                                "origin": "transaction_timeout",
                                "failed_leg_id": leg,
                            }),
                        },
                    ]);
                }
            }
            ok(vec![RuleAction::BeginTermination { reason: Some("timeout".into()) }])
        }),
        // ── timers ──────────────────────────────────────────────────────────
        rule("no-answer", &[], Match::timer().timer_type(TimerType::NoAnswer), |ctx| {
            let leg = ctx.source_leg_id.to_string();
            let mut actions = vec![
                RuleAction::AddCdrEvent { event_type: CdrEventType::Timeout, leg_id: leg.clone(), status_code: None, reason: Some("no_answer_timeout".into()) },
                RuleAction::DestroyLeg { leg_id: leg.clone() },
            ];
            match ctx.call.callback_context() {
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
        // Call-level a-leg setup deadline (armed at route time, cancelled at
        // answer). Deliberately NOT per-b-leg: reroute/failover creates fresh
        // b-legs (each with its own optional NoAnswer), while this caps the
        // caller's TOTAL wait for a final response. It rides the replicated
        // `call.timers` ledger, so a crash → reclaim restores it and a
        // stuck-in-setup call is torn down at the deadline instead of holding
        // its limiter slots until GlobalDuration (the 2026-06-12 endurance
        // zombie: the sip-txn INVITE_INITIAL_TIMEOUT died with the node).
        rule("setup-timeout", &[], Match::timer().timer_type(TimerType::SetupTimeout), |ctx| {
            // Answer raced the fire (e.g. a reclaim restored a stale ledger
            // entry whose cancel was lost with the crashed node): absorb, and
            // scrub the spent entry so a later reclaim cannot re-fire it.
            let answered = ctx.call.a_leg().state == LegState::Confirmed
                || ctx.call.b_legs().iter().any(|b| b.state == LegState::Confirmed);
            if answered {
                return ok(vec![RuleAction::CancelTimer {
                    id: format!("{:?}", TimerType::SetupTimeout),
                }]);
            }
            ok(vec![
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: ctx.call.a_leg().leg_id.clone(),
                    status_code: Some(408),
                    reason: Some("setup_timeout".into()),
                },
                // Final answer to the caller; BeginTermination then CANCELs the
                // pending (trying/early) b-legs and the → terminated invariant
                // settles the obligations (limiter decrements + CDR).
                RuleAction::RespondToALeg {
                    status: 408,
                    reason: "Request Timeout".into(),
                    header_updates: vec![],
                    contacts: vec![],
                },
                RuleAction::BeginTermination { reason: Some("setup-timeout".into()) },
            ])
        }),
        rule("max-duration", &[], Match::timer().timer_type(TimerType::GlobalDuration), |ctx| {
            ok(vec![
                RuleAction::AddCdrEvent { event_type: CdrEventType::Bye, leg_id: ctx.call.a_leg().leg_id.clone(), status_code: None, reason: Some("max_duration".into()) },
                RuleAction::BeginTermination { reason: Some("max-duration".into()) },
            ])
        }),
        rule("keepalive", &[], Match::timer().timer_type(TimerType::Keepalive).call_state(CallModelState::Active), |ctx| {
            let mut actions = Vec::new();
            for leg_id in ctx.call.all_peered_legs() {
                actions.push(RuleAction::SendRequestToLeg { leg_id: leg_id.clone(), method: "OPTIONS".into(), body: vec![], content_type: None });
                actions.push(RuleAction::ScheduleTimer { timer_type: TimerType::KeepaliveTimeout, delay_sec: keepalive_timeout(ctx), leg_id: Some(leg_id) });
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
        // ── un-ACKed 2xx watchdog (RFC 3261 §13.3.1.4) ───────────────────────
        // The caller's ACK has not yet arrived: retransmit the a-leg 2xx and
        // re-arm the cadence. Cancelled by `relay-ack` on the a-leg ACK; bounded
        // by `unacked-2xx-give-up` (the AckTimeout). Only fires while Active.
        rule("unacked-2xx-retransmit", &[], Match::timer().timer_type(TimerType::AckRetransmit).call_state(CallModelState::Active), |_ctx| {
            ok(vec![
                RuleAction::RetransmitALeg2xx,
                RuleAction::ScheduleTimer { timer_type: TimerType::AckRetransmit, delay_sec: ACK_RETRANSMIT_SEC, leg_id: None },
            ])
        }),
        // The give-up deadline (64·T1) elapsed with no a-leg ACK: the caller is
        // gone. Clear the just-created a-leg dialog with a BYE AND tear down the
        // b-leg — without this the answered, bridged call leaks until the 1 h
        // GlobalDuration cap. BeginTermination BYEs every confirmed leg (a-leg +
        // b-leg) and the → terminated invariant settles the obligations; the
        // companion AckRetransmit cadence is reclaimed by the terminal CancelAll.
        rule("unacked-2xx-give-up", &[], Match::timer().timer_type(TimerType::AckTimeout).call_state(CallModelState::Active), |ctx| {
            ok(vec![
                RuleAction::CancelTimer { id: format!("{:?}", TimerType::AckRetransmit) },
                RuleAction::AddCdrEvent { event_type: CdrEventType::Bye, leg_id: ctx.call.a_leg().leg_id.clone(), status_code: None, reason: Some("ack_timeout".into()) },
                RuleAction::BeginTermination { reason: Some("ack-timeout".into()) },
            ])
        }),
        // ── call reaper verdicts (ADR-0020 X1/X6) ───────────────────────────
        // The reaper's sweep / panic-strike verdicts arrive as ordinary
        // InternalEvents and are handled by ordinary CORE rules — the single
        // funnel: force every unresolved leg terminal (no wire traffic — the
        // call is provably dead: its stamp froze past the idle threshold, or
        // its handler panicked), record the reason, and BeginTermination; the
        // invariant then promotes → Terminated and discharges the obligations
        // (CDR + limiter) exactly once. The `discharge` outcome deliberately
        // has NO rule (the router's bypass branch owns it — rules are the
        // thing that failed by then).
        rule(
            "reaper-stale",
            &[],
            Match::internal_event()
                .topic(crate::reaper::REAPER_TOPIC)
                .outcome(crate::reaper::OUTCOME_STALE),
            |ctx| reap_force_terminal(ctx, "reaper-stale"),
        ),
        rule(
            "reaper-fatal-error",
            &[],
            Match::internal_event()
                .topic(crate::reaper::REAPER_TOPIC)
                .outcome(crate::reaper::OUTCOME_FATAL),
            |ctx| reap_force_terminal(ctx, "handler-panic"),
        ),
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
            for leg in std::iter::once(ctx.call.a_leg()).chain(ctx.call.b_legs().iter()) {
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

//! The CORE_LAYER rules of the bridged-call lifecycle: INVITE → 18x → 200 →
//! ACK → in-dialog → BYE, plus CANCEL, b-leg failure, failover resolution, and
//! the housekeeping timers. Rules are registered in priority order: corner
//! cases + failure resolution first (narrow matches), broad relays last;
//! `overrides` removes a displaced rule regardless of order. Which families
//! compose around this list — and in what order — is owned by
//! [`super::compose`].

use call::{ByeDisposition, CdrEventType, Direction, CallModelState, LegDisposition, LegState, TimerType};

use crate::rules::model::{CORE_LAYER, Match, MessageTransform, RuleAction, RuleContext, RuleDefinition, RuleHandleResult};

use super::route_fold::{
    fold_lands_on_going_away_call, parse_header_updates, parse_route_fold,
    route_fold_parity_actions,
};

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

/// Locate the leg carrying the still-pending relayed re-INVITE a CANCEL
/// targets: the pending-relay snapshot whose `inbound_cseq` equals the
/// CANCELled INVITE's CSeq (the canceller's own CSeq space). Returns the
/// target leg id + the snapshot's `outbound_cseq` (the relayed transaction's
/// CSeq on that leg's dialog). At most one relayed INVITE is in flight per
/// call (`reinvite-glare` 491s a second), so the first match is the match;
/// an already-`cancelled` snapshot is skipped (a retransmitted CANCEL must
/// not re-CANCEL).
fn find_pending_relayed_invite(ctx: &RuleContext, inbound_cseq: i64) -> Option<(String, i64)> {
    std::iter::once(ctx.call.a_leg())
        .chain(ctx.call.b_legs().iter())
        .find_map(|leg| {
            leg.dialogs.iter().find_map(|d| {
                d.ext
                    .inbound_pending_requests
                    .iter()
                    .find(|p| {
                        p.method.eq_ignore_ascii_case("INVITE")
                            && p.inbound_cseq == inbound_cseq
                            && !p.cancelled
                    })
                    .map(|p| (leg.leg_id.clone(), p.outbound_cseq))
            })
        })
}

fn keepalive_interval(ctx: &RuleContext) -> i64 {
    // The in-dialog OPTIONS keepalive interval is an operator/worker knob
    // (`B2buaConfig::keepalive_interval_sec`, production default 300 s,
    // `B2BUA_KEEPALIVE_SEC` override), not a per-call feature: a 30 s poke breaks
    // long-hold endurance traffic. The per-call `features` keepalive value is
    // accepted but does not drive the runtime timer.
    ctx.config.keepalive_interval_sec
}
fn keepalive_timeout(ctx: &RuleContext) -> i64 {
    // Grace for the in-dialog OPTIONS 200 before the leg is declared dead and the
    // call is torn down. Operator knob (`B2BUA_KEEPALIVE_TIMEOUT_SEC`, default
    // 32 s) — wide enough that a healthy reclaimed dialog whose keepalive
    // round-trip is still settling across a reboot is not BYE'd as dead.
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
/// `pub(crate)` so the re-INVITE watchdog's first arm (in
/// `actions::relay_response`) shares the one cadence constant with the re-arm
/// rule below.
pub(crate) const ACK_RETRANSMIT_SEC: i64 = 1;

/// Shared body of the reaper-verdict rules (ADR-0020 X1): force every
/// still-unresolved leg terminal (mirroring `is_fully_resolved`, like
/// `terminating-safety-timeout`), record the reason on the CDR, and command
/// termination. No wire messages: the legs were force-resolved above, so
/// `BeginTermination` skips them all and just moves the lifecycle — finalize
/// promotes, the invariant discharges the obligations.
fn reap_force_terminal(ctx: &RuleContext, reason: &'static str) -> Option<RuleHandleResult> {
    let mut actions = Vec::new();
    for leg in std::iter::once(ctx.call.a_leg()).chain(ctx.call.b_legs().iter()) {
        // `leg_is_resolved` also treats a still-`Cancelling` leg as unresolved, so
        // a wedged in-flight CANCEL is force-terminated here too (TerminateLeg
        // clears the `Cancelling` disposition, letting the call finalize).
        if !call::helpers::leg_is_resolved(leg) {
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

/// The CORE_LAYER rule set, in registration (priority) order.
pub(super) fn core_rules() -> Vec<RuleDefinition> {
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
                    RuleAction::AckLeg { leg_id: b.clone(), body: Vec::new(), content_type: None },
                    RuleAction::DestroyLeg { leg_id: b },
                ])
            },
        ),
        // A provisional (1xx) from a callee whose leg is being CANCELLed: the
        // caller's INVITE server transaction has already completed (487 on the
        // CANCEL), so relaying it would put a new 1xx after that final — forbidden
        // (RFC 3261 §13.3.1.1 / §17.2.1). Absorb it; the crossing 2xx/487 is owned
        // by `cancel-200-crossing` / `resolve-cancel-response`. The 1xx sibling of
        // `cancel-200-crossing`; outranks `relay-provisional`, which is
        // disposition-blind.
        rule(
            "absorb-1xx-crossing-cancel",
            &["relay-provisional"],
            Match::response()
                .method("INVITE")
                .status_class(1)
                .leg_disposition(LegDisposition::Cancelling)
                .direction(Direction::FromB),
            |_ctx| ok(vec![]),
        ),
        rule(
            "resolve-cancel-response",
            &["route-failure", "absorb-stale-failure"],
            Match::response()
                .method("INVITE")
                .leg_disposition(LegDisposition::Cancelling)
                .direction(Direction::FromB)
                .filter(|ctx| ctx.response().map(|r| r.status() >= 300).unwrap_or(false)),
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
                .filter(|ctx| ctx.response().map(|r| r.status() >= 300).unwrap_or(false)),
            |_ctx| ok(vec![]),
        ),
        // Re-INVITE glare (RFC 3261 §14.1 / §3.1 of RFC 5407): an INVITE arrives
        // on a dialog that already carries an in-flight inbound INVITE (a
        // re-INVITE we relayed onto this dialog and have not yet seen a final
        // response for) → reject the newcomer 491 Request Pending. More specific
        // than `relay-reinvite` (no filter), so it wins on glare.
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
        // In-dialog UPDATE while the peer side is NOT in a relayable state:
        // no peer leg, the peer leg terminated by a failure whose
        // `/call/failure` reroute is still pending, or a replacement leg whose
        // dialog has no remote tag yet. `relay-update` would either be silently
        // dropped by the relay machinery (tag-less target dialog) or fired into
        // a dead dialog, leaving the requester to time out. Answer **491
        // Request Pending** locally instead (RFC 5407 §3.1 glare treatment —
        // the spec-expected "retry later" during a pending failover; the
        // requester re-UPDATEs once the reroute settles). The condition rides
        // `RuleContext::peer_relay_ready` — the same resolver the relay
        // executor uses — and is part of the rule vocabulary, so a
        // SERVICE_LAYER rule can out-rank this CORE default and own the policy
        // (park, alternate code, …). A legitimate early-dialog UPDATE
        // (RFC 3311 §5.1 — peer early WITH a remote tag) stays relayable and
        // never matches here.
        rule(
            "update-peer-unavailable",
            &["relay-update"],
            Match::request().method("UPDATE").filter(|ctx| !ctx.peer_relay_ready()),
            |_ctx| ok(vec![RuleAction::Respond { status: 491, reason: "Request Pending".into(), body: vec![], content_type: None }]),
        ),
        // Resolve a response to a relayed re-INVITE the originator CANCELled
        // (RFC 3261 §9 — `handle-reinvite-cancel` marked its pending-relay
        // snapshot `cancelled`). The originator's own re-INVITE transaction was
        // already answered by the txn layer (200 to the CANCEL + 487), so the
        // target's final must NOT be relayed:
        //   - 487 / any non-2xx final → drop the snapshot; renegotiation over,
        //     dialog + call intact (§14.1: a failed re-INVITE leaves the
        //     session as it was).
        //   - **crossing 2xx** (the target answered before our CANCEL landed,
        //     §9.1): ACK it on this dialog (quiesces the target's 2xx
        //     retransmissions, §13.2.2.4) and absorb. The call stays up. The
        //     two sides' SDP views may diverge until the next renegotiation
        //     (the target applied the new offer; the canceller kept the old
        //     session) — the deliberate minimal-intervention choice: the
        //     canceller abandoned the renegotiation it initiated, and killing
        //     the call (or minting a resync re-INVITE) costs more than the
        //     transient divergence.
        //   - 1xx → absorb and keep waiting for the final.
        // Outranks `relay-reinvite-response` (whose filter also skips cancelled
        // snapshots) and the INVITE-response fallbacks that would otherwise
        // claim the final.
        rule(
            "resolve-cancelled-reinvite-response",
            &["relay-reinvite-response", "relay-provisional", "confirm-dialog", "route-failure"],
            Match::response().method("INVITE").filter(|ctx| {
                let cseq = match ctx.response() {
                    Some(r) => r.cseq().seq() as i64,
                    None => return false,
                };
                ctx.source_dialog()
                    .and_then(|d| call::helpers::find_pending_request(d, cseq))
                    .map(|p| p.cancelled)
                    .unwrap_or(false)
            }),
            |ctx| {
                let resp = ctx.response()?;
                if resp.status() < 200 {
                    // Provisional on the CANCELled re-INVITE — absorb; the 487
                    // (or crossing 2xx) is still coming.
                    return ok(vec![]);
                }
                let leg = ctx.source_leg_id.to_string();
                let outbound_cseq = resp.cseq().seq() as i64;
                let mut actions = Vec::new();
                if (200..300).contains(&resp.status()) {
                    actions.push(RuleAction::AckLeg { leg_id: leg.clone(), body: Vec::new(), content_type: None });
                }
                actions.push(RuleAction::ResolveCancelledReinvite { leg_id: leg, outbound_cseq });
                ok(actions)
            },
        ),
        // Relay a re-INVITE response (1xx/2xx/3xx+) back to the originator. The
        // source dialog carries a pending-relay snapshot for the response CSeq
        // (captured when the re-INVITE was relayed onto this dialog) — so the
        // relay path rebuilds the response from that snapshot and removes the
        // entry on the final response. Outranks `relay-provisional`,
        // `confirm-dialog` and `route-failure`, which would otherwise claim an
        // INVITE response.
        rule(
            "relay-reinvite-response",
            &["relay-provisional", "confirm-dialog", "route-failure"],
            Match::response().method("INVITE").filter(|ctx| {
                let cseq = match ctx.response() {
                    Some(r) => r.cseq().seq() as i64,
                    None => return false,
                };
                // A `cancelled` snapshot is NOT relayable — its originator was
                // already 487'd by the txn layer when the CANCEL matched; the
                // final resolves via `resolve-cancelled-reinvite-response`.
                ctx.source_dialog()
                    .and_then(|d| call::helpers::find_pending_request(d, cseq))
                    .map(|p| !p.cancelled)
                    .unwrap_or(false)
            }),
            |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
        ),
        // RFC 3261 §13.2.2.4 — re-ACK a **retransmitted 2xx** whose first ACK was
        // lost. The ACK for a 2xx is a UAC-core responsibility and the answerer
        // re-sends its 2xx end-to-end until ACKed (up to its Timer H ≈ 32 s), so
        // the B2BUA — as the ACKing UAC on its own realign/reroute re-INVITE, or
        // when it ACKs a callee's 2xx — MUST re-emit the ACK on the SAME client
        // transaction (reusing the retained `ack_branch` + the INVITE CSeq) for
        // every retransmit — otherwise a single lost ACK on a realign/reroute
        // (or initial) leg strands the answerer's INVITE server txn and the
        // confirmed call is never fully reaped (leak) or times out late under
        // real-network packet loss. This is the b-leg / realign twin of the
        // a-leg `unacked-2xx-retransmit` (which retransmits the B2BUA's *own* 2xx
        // to a silent caller). Delayed-offer answer bodies are not replayed on the
        // re-ACK (the realign/reroute leak paths are all offer-in-INVITE, so their
        // ACKs are bodyless); the branch + CSeq are what quiesce the answerer.
        //
        // Fires ONLY on a genuine retransmit that nothing else claims: the source
        // dialog has a retained `ack_branch` (so its current INVITE's 2xx was
        // already ACKed — the field is reset on every new INVITE txn), the
        // response echoes that INVITE's CSeq, and no pending-relay snapshot is open
        // (a first-time re-INVITE final is claimed by `relay-reinvite-response` /
        // the realign-200 rules, whose `ack_branch` is still `None`). Absorbs
        // (AckLeg only — no relay to the peer, which already saw the first final).
        rule(
            "re-ack-retransmitted-2xx",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .leg_states(&[LegState::Confirmed])
                .filter(|ctx| {
                    let Some(d) = ctx.source_dialog() else { return false };
                    if d.ext.ack_branch.is_none() {
                        return false;
                    }
                    let Some(resp) = ctx.response() else { return false };
                    let cseq = resp.cseq().seq() as i64;
                    crate::rules::relay::acked_invite_cseq(d) == Some(resp.cseq().seq())
                        && call::helpers::find_pending_request(d, cseq).is_none()
                }),
            |ctx| {
                ok(vec![RuleAction::AckLeg {
                    leg_id: ctx.source_leg_id.to_string(),
                    body: Vec::new(),
                    content_type: None,
                }])
            },
        ),
        // ── dialog ──────────────────────────────────────────────────────────
        rule(
            "relay-provisional",
            &[],
            Match::response().method("INVITE").status_class(1).direction(Direction::FromB),
            |ctx| {
                let b = ctx.source_leg_id.to_string();
                let status = ctx.response().map(|r| r.status() as i64);
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
                    // The failure-headers image is consult-scoped (ADR-0017
                    // X2); an answered call carries none of it in replicated
                    // state.
                    RuleAction::MergeCallExt {
                        ext: crate::rules::relay::failure_headers_ext(None),
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
                .methods(&["OPTIONS", "INFO", "PRACK", "UPDATE", "REFER", "MESSAGE", "SUBSCRIBE", "NOTIFY"])
                .status_class(2),
            |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
        ),
        // A relayed non-INVITE request's **non-2xx final** is relayed back to
        // its requester — plain transaction-layer symmetry (RFC 3261 §8.1.3.3).
        // The non-INVITE sibling of `relay-reinvite-response`
        // (INVITE) alongside `relay-non-invite-200` (the 2xx half): without it
        // the far end's 481/488/491… to a relayed UPDATE/INFO was silently
        // dropped and the requester timed out. Matches ONLY when the source
        // dialog holds a pending-relay snapshot for the response CSeq — i.e. a
        // transaction WE relayed. A B2BUA-originated request (keepalive
        // OPTIONS, relayFirst18x PRACK, REFER-progress NOTIFY) leaves no
        // snapshot, so its failures keep their own rules — in particular a
        // keepalive-OPTIONS 481 still reaches `handle-481`'s teardown, which
        // this rule outranks only for relayed transactions. Like a failed
        // re-INVITE (§14.1), a failed relayed non-INVITE leaves the dialog and
        // the call as they were: report the failure to the requester, nothing
        // more. (BYE never takes this path — it is answered locally and leaves
        // no snapshot; the relay executor removes the snapshot on this final.)
        rule(
            "relay-non-invite-failure",
            &["handle-481"],
            Match::response()
                .methods(&["OPTIONS", "INFO", "PRACK", "UPDATE", "REFER", "MESSAGE", "SUBSCRIBE", "NOTIFY"])
                .filter(|ctx| {
                    let Some(resp) = ctx.response() else {
                        return false;
                    };
                    if resp.status() < 300 {
                        return false;
                    }
                    let cseq = resp.cseq().seq() as i64;
                    // Fork-correct dialog pick (mirrors `relay_response`): the
                    // responder's To-tag selects the exact source dialog, else
                    // the confirmed/first one.
                    let to_tag = resp.to().tag().map(str::to_owned).unwrap_or_default();
                    ctx.source_leg()
                        .and_then(|leg| call::helpers::find_dialog_by_to_tag(leg, &to_tag))
                        .or_else(|| ctx.source_dialog())
                        .and_then(|d| call::helpers::find_pending_request(d, cseq))
                        .is_some()
                }),
            |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
        ),
        // ── failure ─────────────────────────────────────────────────────────
        rule(
            "route-failure",
            &[],
            Match::response()
                .method("INVITE")
                .direction(Direction::FromB)
                .filter(|ctx| ctx.response().map(|r| r.status() >= 300).unwrap_or(false)),
            |ctx| {
                let b = ctx.source_leg_id.to_string();
                let (status, reason) = ctx
                    .response()
                    .map(|r| (r.status() as i64, r.reason().to_string()))
                    .unwrap_or((500, "Server Error".into()));
                // Tear the failed leg down + record the reject. The relay/terminate
                // (or failover) is decided next.
                let mut actions = vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Reject,
                        leg_id: b.clone(),
                        status_code: Some(status),
                        reason: Some(reason.to_string()),
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
                                r.headers()
                                    .iter()
                                    .filter(|h| {
                                        !crate::initial_invite::STANDARD_HEADERS
                                            .iter()
                                            .any(|n| n.matches(&h.name))
                                    })
                                    .map(|h| serde_json::json!([h.name, h.value]))
                                    .collect()
                            })
                            .unwrap_or_default();
                        // The same final's RELAYABLE image is kept on the call
                        // (distinct from the payload above — `relayable_headers`
                        // withholds credentials, per-leg negotiation and a
                        // concealed identity), so the a-facing final the
                        // decision authors carries what the callee stated. It
                        // is restated on EVERY consult, so a superseded
                        // attempt's image never answers a later failure.
                        actions.push(RuleAction::MergeCallExt {
                            ext: crate::rules::relay::failure_headers_ext(ctx.response()),
                        });
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
        // the new leg's To-tag stays the first 180's).
        rule(
            "failover-create-leg",
            &[],
            Match::internal_event()
                .topic("call-failure-result")
                .outcome("failover"),
            |ctx| {
                // Fold landed on a going-away call (069): the caller already
                // holds its final — drop whole, no leg toward a caller-less
                // callee (see `fold_lands_on_going_away_call`).
                if fold_lands_on_going_away_call(ctx) {
                    return ok(vec![]);
                }
                let payload = match ctx.event {
                    crate::event::CallEvent::InternalEvent { payload, .. } => payload,
                    _ => return None,
                };
                // ── failover/initial-route parity ────────────────────────────
                // The same decision response must be honored the same way on
                // both paths, so the reroute applies what `apply_route` applies
                // (one shared parser + parity-action builder, also used by the
                // `release-reroute` fold): features (incl. the GlobalDuration
                // re-arm), service_ext, subscriptions, update_body, and the
                // limiter holds the router's fold already admitted.
                let fold = parse_route_fold(payload)?;
                let failed_leg_id = payload.get("failed_leg_id").and_then(|v| v.as_str()).unwrap_or("");

                let mut actions = Vec::new();
                // Cancel the failed leg's no-answer timer (a reject can beat it;
                // for the no-answer trigger the timer already fired — harmless).
                if !failed_leg_id.is_empty() {
                    actions.push(RuleAction::CancelTimer { id: format!("NoAnswer:{failed_leg_id}") });
                }
                let no_answer = fold
                    .no_answer
                    .or(fold.features.as_ref().and_then(|f| f.no_answer_timeout_sec));
                actions.extend(route_fold_parity_actions(&fold, ctx));
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
                // Fold landed on a going-away call (069): the caller already
                // holds its final — no relayed failure, no re-termination.
                if fold_lands_on_going_away_call(ctx) {
                    return ok(vec![]);
                }
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
                // Fold landed on a going-away call (069): no second final on
                // the a-leg's completed transaction (RFC 3261 §17.2.1).
                if fold_lands_on_going_away_call(ctx) {
                    return ok(vec![]);
                }
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
                // Fold landed on a going-away call (069): no second final on
                // the a-leg's completed transaction (RFC 3261 §17.2.1).
                if fold_lands_on_going_away_call(ctx) {
                    return ok(vec![]);
                }
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
            // and `relay-non-invite-200` forwards the 200 to the peer.
            Match::response().method("OPTIONS").status_class(2).filter(|ctx| {
                let cseq = ctx.response().map(|r| r.cseq().seq() as i64);
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
            &["relay-non-invite-200"],
            // Only a B2BUA-originated NOTIFY is absorbed: the `referTransfer`
            // machine's `SendNotify` (refer-progress sipfrag toward the referrer)
            // leaves no pending-relay snapshot on the source dialog. A **relayed**
            // NOTIFY — the transparent-REFER path forwards the transferee's
            // implicit-subscription NOTIFYs (RFC 3515) end-to-end — DOES leave one
            // (matching the response CSeq) → this declines and `relay-non-invite-200`
            // forwards the 200 back to the requester. Mirrors `absorb-options-200`.
            Match::response().method("NOTIFY").status_class(2).filter(|ctx| {
                let cseq = ctx.response().map(|r| r.cseq().seq() as i64);
                match (ctx.source_dialog(), cseq) {
                    (Some(d), Some(seq)) => call::helpers::find_pending_request(d, seq).is_none(),
                    _ => true,
                }
            }),
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
                // RFC 3261 §13.3.1.4 (in-dialog): if this a-leg ACK is for the
                // pending re-INVITE 2xx (its CSeq matches the cached snapshot),
                // quiesce the re-INVITE un-ACKed-2xx watchdog and discharge the
                // obligation. CSeq-matched so a retransmitted *initial* ACK
                // (a lower CSeq) cannot prematurely cancel it; at most one
                // re-INVITE is ever pending (`reinvite-glare` 491s a second).
                let acks_pending_reinvite = ctx
                    .request()
                    .map(|r| r.cseq().seq() as i64)
                    .and_then(|c| {
                        ctx.call
                            .a_leg()
                            .dialogs
                            .first()
                            .and_then(|d| d.ext.pending_reinvite_2xx.as_ref())
                            .map(|p| p.cseq == c)
                    })
                    .unwrap_or(false);
                if acks_pending_reinvite {
                    actions.push(RuleAction::CancelTimer { id: format!("{:?}", TimerType::ReinviteAckRetransmit) });
                    actions.push(RuleAction::CancelTimer { id: format!("{:?}", TimerType::ReinviteAckTimeout) });
                    actions.push(RuleAction::ClearPendingReinvite2xx);
                }
            }
            ok(actions)
        }),
        rule("relay-bye", &[], Match::request().method("BYE").call_state(CallModelState::Active), |ctx| {
            // Pre-mark the BYE-sending leg `bye_received` (RFC 3261 §15.1.2) so the
            // subsequent begin-termination skips it (no duplicate BYE back to the
            // sender) and only tears down the peer.
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
        // Transparent in-dialog REFER relay: forward a
        // REFER to the peer leg like INFO/MESSAGE. This is the FALLBACK for an
        // *unsubscribed* transfer — with the `refer_transfer` seed PRESENT (default
        // composition) `transfer-intercept-refer` (also CORE, registered earlier)
        // out-ranks this by registration order and still intercepts; with the seed
        // excluded (a downstream owns REFER), this relays it transparently. The
        // 202/failure finals ride `relay-non-invite-200` / `relay-non-invite-failure`
        // (REFER is in both method sets), and the transferee's implicit-subscription
        // NOTIFYs ride `relay-notify` — the whole RFC 3515 exchange passes through.
        rule("relay-refer", &[], Match::request().method("REFER"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        // Transparent in-dialog NOTIFY relay: forward an in-dialog NOTIFY request
        // to the peer leg (the implicit REFER subscription's progress reports ride
        // the dialog end-to-end on the transparent-transfer path). Its 200/failure
        // finals relay back via `relay-non-invite-200`/`-failure` (NOTIFY is in both
        // sets); `absorb-notify-200`'s snapshot filter keeps a B2BUA-*originated*
        // NOTIFY's 200 absorbed. A SERVICE_LAYER rule (e.g. a subscribed transfer
        // machine that owns NOTIFY) out-ranks this CORE default when active.
        rule("relay-notify", &[], Match::request().method("NOTIFY"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        // ── lifecycle ───────────────────────────────────────────────────────
        // RFC 3261 §9: a CANCEL targets the one pending INVITE **transaction**
        // it matched — cancelling a re-INVITE ends that renegotiation, not the
        // call. The txn layer already answered the canceller (200 to the
        // CANCEL, 487 to the re-INVITE) and told us which INVITE it matched
        // (`in_dialog` = its To carried a tag ⇒ a re-INVITE). Here we CANCEL
        // the *relayed* re-INVITE toward the peer (transaction-scoped: no leg
        // state change, `CancelPendingReinvite`) and leave the dialog and the
        // call up; the peer's 487 — or a crossing 200 — is resolved by
        // `resolve-cancelled-reinvite-response`. If no un-cancelled pending
        // relayed INVITE exists (the peer's final was already relayed — the
        // CANCEL lost the race end-to-end), there is nothing left to cancel:
        // absorb and keep the call (§9.2 — a CANCEL never affects a completed
        // transaction). Outranks `handle-cancel`, whose unconditional teardown
        // is the correct semantics ONLY for the initial INVITE.
        rule(
            "handle-reinvite-cancel",
            &["handle-cancel"],
            Match::cancelled().filter(|ctx| ctx.cancelled_in_dialog()),
            |ctx| {
                let Some((leg_id, outbound_cseq)) =
                    ctx.cancelled_invite_cseq().and_then(|c| find_pending_relayed_invite(ctx, c))
                else {
                    // Already answered end-to-end (or never relayed): no-op —
                    // the call must survive an in-dialog CANCEL regardless.
                    return ok(vec![]);
                };
                ok(vec![
                    RuleAction::CancelPendingReinvite {
                        leg_id: leg_id.clone(),
                        outbound_cseq,
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Cancel,
                        leg_id,
                        status_code: None,
                        reason: Some("reinvite_cancelled".into()),
                    },
                ])
            },
        ),
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
            // the classic failover case. Mirror the
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
                // A leg already going away (caller-CANCELed → `Cancelling`, or
                // the whole call Terminating) makes no forward progress on its
                // transaction timeout: no /calls/failure consult, no
                // BeginTermination re-arm of the safety timer. The dead
                // transaction resolves the leg locally (TerminateLeg clears
                // `Cancelling`), letting the deferred termination finalize.
                let going_away = ctx
                    .source_leg()
                    .is_some_and(|l| call::helpers::leg_is_going_away(ctx.call.state(), l));
                if going_away {
                    return ok(vec![RuleAction::TerminateLeg {
                        leg_id: ctx.source_leg_id.to_string(),
                        bye_disposition: Some(ByeDisposition::Cancelled),
                    }]);
                }
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
                        // A blackholed hop drew no final: this consult states an
                        // empty relayable image, so an earlier attempt's headers
                        // cannot answer it.
                        RuleAction::MergeCallExt { ext: crate::rules::relay::failure_headers_ext(None) },
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
            // NoAnswer is armed PER B-LEG: a fire for leg X is spent iff X is
            // no longer awaiting an answer — Confirmed, absent from the call
            // (reclaim can restore a stale entry whose cancel died with the
            // crashed node), or already going away (`leg_is_going_away`: a
            // caller-CANCELed leg still reads `Trying` while its disposition is
            // `Cancelling`, and a terminating call makes no forward progress —
            // no /calls/failure consult, no new final on the a-leg's completed
            // transaction). Absorb and scrub so a later reclaim cannot re-fire
            // it. Other legs' states are irrelevant to X's fire.
            let spent = match ctx.source_leg() {
                Some(leg) => leg.state == LegState::Confirmed
                    || call::helpers::leg_is_going_away(ctx.call.state(), leg),
                None => true,
            };
            if spent {
                return ok(vec![RuleAction::cancel_timer(
                    &TimerType::NoAnswer,
                    Some(ctx.source_leg_id),
                )]);
            }
            let leg = ctx.source_leg_id.to_string();
            let mut actions = vec![
                RuleAction::AddCdrEvent { event_type: CdrEventType::Timeout, leg_id: leg.clone(), status_code: None, reason: Some("no_answer_timeout".into()) },
                RuleAction::DestroyLeg { leg_id: leg.clone() },
            ];
            match ctx.call.callback_context() {
                // Failover-capable → ask /call/failure (origin no_answer_timeout);
                // the result drives `failover-create-leg` / `failover-terminate`.
                // A ring-forever hop drew no final, so this consult states an
                // EMPTY relayable image: the final it authors speaks for a peer
                // that never answered, not for an earlier attempt that did.
                Some(cbctx) => actions.extend([
                    RuleAction::MergeCallExt { ext: crate::rules::relay::failure_headers_ext(None) },
                    RuleAction::FailureAsyncHttp {
                        request: serde_json::json!({
                            "callback_context": cbctx,
                            "origin": "no_answer_timeout",
                            "failed_leg_id": leg,
                        }),
                    },
                ]),
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
        // its limiter slots until GlobalDuration — the sip-txn setup timeout
        // is process-local and dies with a crashed node; this one survives.
        rule("setup-timeout", &[], Match::timer().timer_type(TimerType::SetupTimeout), |ctx| {
            // Answer raced the fire (e.g. a reclaim restored a stale ledger
            // entry whose cancel was lost with the crashed node), or the call
            // is already going away (a terminating call authors no new final
            // on the a-leg's completed transaction): absorb, and scrub the
            // spent entry so a later reclaim cannot re-fire it.
            let answered = ctx.call.a_leg().state == LegState::Confirmed
                || ctx.call.b_legs().iter().any(|b| b.state == LegState::Confirmed);
            let spent = answered
                || matches!(
                    ctx.call.state(),
                    CallModelState::Terminating | CallModelState::Terminated
                );
            if spent {
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
            // A **subscribed** max-call-duration on an ANSWERED
            // call with a callback_context consults the engine (`call_release`
            // via `ReleaseAsyncHttp` → `call-release-result`: release |
            // reroute) instead of tearing down locally. Everything else keeps
            // today's unconditional local teardown:
            //   - unsubscribed (the default) / no callback_context — the
            //     no-subscription fallback the request mandates;
            //   - a call still in setup (`GlobalDuration` is armed at route
            //     time as a backstop; the release/reroute domain is
            //     established calls only);
            //   - an in-flight reroute whose re-armed cap expired mid-realign
            //     (never consult recursively — the treatment already failed to
            //     settle within its own cap).
            // The consult itself is deadline-bounded (`DeadlineDecisionEngine`
            // wraps `call_release`); its error/timeout folds outcome `release`
            // → the `release-result-release` rule performs exactly this local
            // teardown, so the fail-safe is this same path.
            //
            // A cap crossing the terminating window (the caller BYE'd just
            // before it, or a reclaim restored a stale entry) is SPENT: the
            // call is already going away, so no `call_release` consult (a
            // `reroute` outcome would dial a fresh b-leg on a terminating
            // call) and no BeginTermination re-arm of the safety timer —
            // absorb, and scrub the entry so a later reclaim cannot re-fire it.
            if matches!(
                ctx.call.state(),
                CallModelState::Terminating | CallModelState::Terminated
            ) {
                return ok(vec![RuleAction::CancelTimer {
                    id: format!("{:?}", TimerType::GlobalDuration),
                }]);
            }
            let answered = ctx.call.a_leg().state == LegState::Confirmed;
            if answered
                && !ctx.call.reroute_active()
                && ctx.call.subscribed(call::ReleaseEventKind::MaxCallDuration)
            {
                if let Some(cbctx) = ctx.call.callback_context() {
                    return ok(vec![RuleAction::ReleaseAsyncHttp {
                        request: serde_json::json!({
                            "callback_context": cbctx,
                            "event": "max_call_duration",
                        }),
                    }]);
                }
            }
            ok(vec![
                RuleAction::AddCdrEvent { event_type: CdrEventType::Bye, leg_id: ctx.call.a_leg().leg_id.clone(), status_code: None, reason: Some("max_duration".into()) },
                RuleAction::BeginTermination { reason: Some("max-duration".into()) },
            ])
        }),
        rule("keepalive", &[], Match::timer().timer_type(TimerType::Keepalive).call_state(CallModelState::Active), |ctx| {
            let mut actions = Vec::new();
            for leg_id in ctx.call.all_peered_legs() {
                actions.push(RuleAction::SendRequestToLeg { leg_id: leg_id.clone(), method: "OPTIONS".into(), body: vec![], content_type: None, headers: vec![] });
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
        // ── un-ACKed re-INVITE 2xx watchdog (RFC 3261 §13.3.1.4, in-dialog) ──
        // The re-INVITE twin of `unacked-2xx-retransmit`: the originator's ACK
        // for the relayed re-INVITE 2xx has not yet arrived, so retransmit the
        // cached a-leg re-INVITE 2xx (raw, byte-faithful) and re-arm the cadence.
        // Cancelled by `relay-ack` on the CSeq-matched a-leg ACK; bounded by
        // `unacked-reinvite-2xx-give-up`. Only fires while Active — a `Terminating`
        // call's cadence stops here and the terminal CancelAll reclaims it.
        rule("unacked-reinvite-2xx-retransmit", &[], Match::timer().timer_type(TimerType::ReinviteAckRetransmit).call_state(CallModelState::Active), |_ctx| {
            ok(vec![
                RuleAction::RetransmitALegReinvite2xx,
                RuleAction::ScheduleTimer { timer_type: TimerType::ReinviteAckRetransmit, delay_sec: ACK_RETRANSMIT_SEC, leg_id: None },
            ])
        }),
        // The re-INVITE 2xx give-up deadline (64·T1) elapsed with no a-leg ACK: a
        // permanently-lost re-INVITE ACK must never retransmit forever. Cancel the
        // cadence and tear the call down (BeginTermination BYEs both confirmed
        // legs; the → terminated invariant settles the obligations). The re-INVITE
        // twin of `unacked-2xx-give-up`.
        rule("unacked-reinvite-2xx-give-up", &[], Match::timer().timer_type(TimerType::ReinviteAckTimeout).call_state(CallModelState::Active), |ctx| {
            ok(vec![
                RuleAction::CancelTimer { id: format!("{:?}", TimerType::ReinviteAckRetransmit) },
                RuleAction::ClearPendingReinvite2xx,
                RuleAction::AddCdrEvent { event_type: CdrEventType::Bye, leg_id: ctx.call.a_leg().leg_id.clone(), status_code: None, reason: Some("reinvite_ack_timeout".into()) },
                RuleAction::BeginTermination { reason: Some("reinvite-ack-timeout".into()) },
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
            // the call — its `active_calls` slot AND its memory — leaks forever.
            // Force every still-unresolved leg terminal (mirroring the
            // `is_fully_resolved` predicate) so the invariant promotes
            // Terminating→Terminated→RemoveCall and the call is reaped + the
            // replication delete propagates. If the call already resolved, the
            // loop yields no actions and this stays the harmless canary it was.
            let mut actions = Vec::new();
            for leg in std::iter::once(ctx.call.a_leg()).chain(ctx.call.b_legs().iter()) {
                // Force every still-unresolved leg terminal. `leg_is_resolved` also
                // covers a leg wedged in `Cancelling` (an internal CANCEL whose 487
                // / crossing 200 never arrived): TerminateLeg clears the disposition
                // so the deferred termination can finally promote → RemoveCall.
                if !call::helpers::leg_is_resolved(leg) {
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

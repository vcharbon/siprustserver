//! `promote18xPemTo200` — SERVICE_LAYER early-media service. Port of
//! `src/b2bua/rules/custom/promote18xPemTo200.ts`.
//!
//! Activated by `features.relay_first_18x_to_180` strategy `promote-pem-to-200`.
//! Promotes Bob's first `183 Session Progress + SDP + P-Early-Media` (RFC 5009)
//! into a synthetic 200 OK toward Alice; opens a promotion window during which
//! Alice's in-dialog requests are gated; on Bob's real 200 it confirms silently
//! and, if Bob's SDP differs from the promoted SDP (`sdp_media_equivalent`),
//! resyncs Alice with a B2BUA-originated re-INVITE. The per-call state lives on
//! `Call.promote_pem` (the typed slice that replaces the TS `PemCallExt`).

use call::features::RelayFirst18xStrategy;
use call::{CdrEventType, Direction, LegDisposition, LegState, PromotePemState, TimerType};
use sip_message::message_helpers::{get_header, get_headers};
use sip_message::SipResponse;

use super::model::{
    Match, MessageTransform, RuleAction, RuleContext, RuleDefinition, RuleHandleResult,
    SERVICE_LAYER,
};
use super::sdp_diff::sdp_media_equivalent;

/// RFC 3261 §13.3.1 / §20.5: methods the B2BUA relays end-to-end, advertised on
/// the synthetic 200 OK / resync re-INVITE toward Alice.
const B2BUA_ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, REFER, PRACK, MESSAGE, NOTIFY";
/// RFC 3261 §20.37: option-tags the B2BUA understands. 100rel is OMITTED — Alice
/// never saw a reliable provisional from us.
const B2BUA_SUPPORTED_NO_100REL: &str = "timer, replaces";

fn rule(
    id: &'static str,
    overrides: &'static [&'static str],
    matcher: Match,
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
) -> RuleDefinition {
    RuleDefinition::core(id, SERVICE_LAYER, overrides, matcher, handle)
}

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

fn has_p_early_media(resp: &SipResponse) -> bool {
    get_header(&resp.headers, "p-early-media").is_some()
}

/// RFC 3326 Reason value from a SIP status + phrase.
fn reason_header(status: u16, phrase: &str) -> String {
    let text = phrase.replace('"', "");
    let text = if text.is_empty() { "Unspecified" } else { &text };
    format!("SIP ;cause={status};text=\"{text}\"")
}

/// The PEM service owns this call.
fn promote_pem_active(ctx: &RuleContext) -> bool {
    call::helpers::relay_first_18x_strategy(ctx.call) == Some(RelayFirst18xStrategy::PromotePemTo200)
}

fn promoted(ctx: &RuleContext) -> bool {
    call::helpers::promote_pem_promoted(ctx.call)
}

fn window_open(ctx: &RuleContext) -> bool {
    call::helpers::promote_pem_window_open(ctx.call)
}

/// Allow + Supported header updates for messages we mint toward Alice.
fn a_facing_advert() -> Vec<(&'static str, String)> {
    vec![
        ("Allow", B2BUA_ALLOW.to_string()),
        ("Supported", B2BUA_SUPPORTED_NO_100REL.to_string()),
    ]
}

/// The `promote18xPemTo200` SERVICE_LAYER rules. Dormant unless the call
/// activates the `promote-pem-to-200` strategy; SERVICE_LAYER ranks them above
/// the CORE rules they displace, and each always consumes.
pub fn promote_pem_rules() -> Vec<RuleDefinition> {
    vec![
        // ── Rule 1: promote-183-pem (beats relay-provisional by layer) ───────
        rule(
            "promote-183-pem",
            &[],
            Match::response()
                .method("INVITE")
                .status_code(183)
                .direction(Direction::FromB)
                .filter(|ctx| {
                    if !promote_pem_active(ctx) || promoted(ctx) {
                        return false;
                    }
                    match ctx.response() {
                        Some(r) => !r.body.is_empty() && has_p_early_media(r),
                        None => false,
                    }
                }),
            |ctx| {
                let resp = ctx.response()?;
                let b_tag = resp.to.tag.clone().unwrap_or_default();
                let rseq = reliable_rseq(resp);
                let invite_cseq = resp.cseq.seq as i64;
                let leg = ctx.source_leg_id.to_string();
                let promoted_sdp = resp.body.clone();

                // 183 → 200 OK on the wire toward Alice: drop Require/RSeq
                // (+ P-Early-Media is not in the relay passthrough set, so it
                // never reaches Alice), stamp Allow + Supported, keep the SDP.
                // RelayFirstBare180 already mints the a-facing tag + seeds the
                // tag map; here we relay as a 200 carrying the body, so we mint
                // the tag via the relay path's reliable-1xx tracking instead and
                // pre-seed by reusing the default a-dialog tag continuity.
                let transform = MessageTransform {
                    status: Some(200),
                    reason: Some("OK".to_string()),
                    drop_body: false,
                    remove_headers: vec!["Require", "RSeq"],
                    add_headers: a_facing_advert(),
                };

                let mut actions = vec![
                    // Establish the a-leg confirmed dialog so Alice's ACK +
                    // in-dialog flow sees a confirmed UAS dialog. Not bridged yet
                    // (b is still early).
                    RuleAction::UpdateLegState {
                        leg_id: "a".to_string(),
                        state: LegState::Confirmed,
                        disposition: None,
                    },
                    RuleAction::RelayToPeer { transform },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Provisional,
                        leg_id: leg.clone(),
                        status_code: Some(200),
                        reason: Some("promote-pem-to-200".to_string()),
                    },
                ];
                if let Some(rseq) = rseq {
                    actions.push(RuleAction::SendPrackToLeg {
                        leg_id: leg,
                        rseq,
                        invite_cseq,
                        b_tag,
                    });
                }
                actions.push(RuleAction::SetPromotePem {
                    state: Some(PromotePemState {
                        promoted: true,
                        promoted_sdp,
                        window_open: true,
                        resync_reinvite_cseq: None,
                    }),
                });
                ok(actions)
            },
        ),
        // ── Rule 2: suppress-post-promote-18x (beats relay-provisional) ──────
        rule(
            "suppress-post-promote-18x",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(1)
                .direction(Direction::FromB)
                .filter(|ctx| promote_pem_active(ctx) && promoted(ctx)),
            |ctx| {
                let resp = ctx.response()?;
                let b_tag = resp.to.tag.clone().unwrap_or_default();
                let rseq = reliable_rseq(resp);
                let invite_cseq = resp.cseq.seq as i64;
                let leg = ctx.source_leg_id.to_string();
                let mut actions = vec![RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Provisional,
                    leg_id: leg.clone(),
                    status_code: Some(resp.status as i64),
                    reason: Some("promote-pem-to-200:suppressed".to_string()),
                }];
                if let Some(rseq) = rseq {
                    actions.push(RuleAction::SendPrackToLeg {
                        leg_id: leg,
                        rseq,
                        invite_cseq,
                        b_tag,
                    });
                }
                ok(actions)
            },
        ),
        // ── Rule 3: confirm-after-promote (beats confirm-dialog) ─────────────
        rule(
            "confirm-after-promote",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .leg_states(&[LegState::Trying, LegState::Early])
                .direction(Direction::FromB)
                .filter(|ctx| promote_pem_active(ctx) && promoted(ctx)),
            |ctx| {
                let resp = ctx.response()?;
                let b = ctx.source_leg_id.to_string();
                let a = ctx.call.a_leg.leg_id.clone();
                let b_tag = resp.to.tag.clone().unwrap_or_default();
                let final_sdp = resp.body.clone();
                let state = call::helpers::promote_pem_state(ctx.call).cloned().unwrap_or_default();
                let promoted_sdp = state.promoted_sdp.clone();

                // Re-seed the (bLegId, winningBTag) → aFacingTag mapping under the
                // winning fork's tag using the SAME a-facing tag pinned at
                // promotion (forking). confirm-dialog reuses a pre-seeded a-tag
                // via find_by_b_tag, so this keeps Alice's identity stable.
                let existing = call::helpers::find_by_b_tag(ctx.call, &b, &b_tag);
                let seeded = ctx.call.tag_map.iter().find(|m| m.b_leg_id == b);
                let a_facing = existing
                    .map(|m| m.a_tag.clone())
                    .or_else(|| seeded.map(|m| m.a_tag.clone()));

                let mut actions: Vec<RuleAction> = Vec::new();
                if existing.is_none() {
                    if let Some(a_tag) = &a_facing {
                        actions.push(RuleAction::AddTagMapping {
                            a_tag: a_tag.clone(),
                            b_leg_id: b.clone(),
                            b_tag: b_tag.clone(),
                        });
                    }
                }

                // Confirm b, ACK b locally (no end-to-end ACK; Alice already
                // ACK'd the synthetic 200), merge a↔b. NO relay-to-peer.
                actions.push(RuleAction::UpdateLegState {
                    leg_id: b.clone(),
                    state: LegState::Confirmed,
                    disposition: Some(LegDisposition::Bridged),
                });
                actions.push(RuleAction::ConfirmDialog { leg_id: b.clone() });
                actions.push(RuleAction::AckLeg { leg_id: b.clone() });
                actions.push(RuleAction::Merge { leg_a: a.clone(), leg_b: b.clone() });
                for other in &ctx.call.b_legs {
                    if other.leg_id != b && other.state != LegState::Terminated {
                        actions.push(RuleAction::DestroyLeg { leg_id: other.leg_id.clone() });
                    }
                }
                actions.push(RuleAction::CancelTimer { id: format!("NoAnswer:{b}") });
                actions.push(RuleAction::ScheduleTimer {
                    timer_type: TimerType::GlobalDuration,
                    delay_sec: max_duration(ctx),
                    leg_id: None,
                });
                actions.push(RuleAction::ScheduleTimer {
                    timer_type: TimerType::Keepalive,
                    delay_sec: keepalive_interval(ctx),
                    leg_id: None,
                });
                actions.push(RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Answer,
                    leg_id: b.clone(),
                    status_code: Some(200),
                    reason: None,
                });

                // SDP diff. Equal → silent bridge, window closes. Different →
                // resync re-INVITE on the a-leg with b's SDP.
                if sdp_media_equivalent(&promoted_sdp, &final_sdp) {
                    actions.push(RuleAction::SetPromotePem { state: None });
                    return ok(actions);
                }
                let a_dialog_cseq = ctx
                    .call
                    .a_leg
                    .dialogs
                    .first()
                    .map(|d| d.sip.local_cseq)
                    .unwrap_or(0);
                let next_cseq = a_dialog_cseq + 1;
                actions.push(RuleAction::SendReinvite {
                    leg_id: a,
                    body: final_sdp,
                    add_headers: a_facing_advert(),
                });
                actions.push(RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Provisional,
                    leg_id: "a".to_string(),
                    status_code: Some(0),
                    reason: Some("promote-pem-to-200:resync-reinvite".to_string()),
                });
                actions.push(RuleAction::SetPromotePem {
                    state: Some(PromotePemState {
                        resync_reinvite_cseq: Some(next_cseq),
                        ..state
                    }),
                });
                ok(actions)
            },
        ),
        // ── Rule 4: resync-reinvite-response (from-a INVITE response) ────────
        rule(
            "promote-resync-reinvite-response",
            &[],
            Match::response()
                .method("INVITE")
                .direction(Direction::FromA)
                .filter(|ctx| {
                    if !promote_pem_active(ctx) {
                        return false;
                    }
                    let expected = call::helpers::promote_pem_state(ctx.call)
                        .and_then(|s| s.resync_reinvite_cseq);
                    match (ctx.response(), expected) {
                        (Some(r), Some(c)) => r.cseq.seq as i64 == c,
                        _ => false,
                    }
                }),
            |ctx| {
                let resp = ctx.response()?;
                // Provisional — keep waiting (leave unclaimed via empty action set
                // is not possible here since we consume; emit no effect).
                if resp.status < 200 {
                    return ok(vec![]);
                }
                if resp.status < 300 {
                    return ok(vec![
                        RuleAction::AckLeg { leg_id: "a".to_string() },
                        RuleAction::AddCdrEvent {
                            event_type: CdrEventType::Answer,
                            leg_id: "a".to_string(),
                            status_code: Some(resp.status as i64),
                            reason: Some("promote-pem-to-200:resync-success".to_string()),
                        },
                        RuleAction::SetPromotePem { state: None },
                    ]);
                }
                // 3xx-6xx — Alice and Bob disagree on SDP. BYE both with Reason.
                let reason = reason_header(resp.status, &resp.reason);
                ok(vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Reject,
                        leg_id: "a".to_string(),
                        status_code: Some(resp.status as i64),
                        reason: Some(format!("promote-pem-to-200:resync-failed:{reason}")),
                    },
                    RuleAction::SetPromotePem { state: None },
                    RuleAction::BeginTermination { reason: Some(reason) },
                ])
            },
        ),
        // ── Rule 5: reject A re-INVITE/UPDATE during window → 491 ────────────
        rule(
            "promote-reject-a-reinvite-update",
            &[],
            Match::request()
                .methods(&["INVITE", "UPDATE"])
                .direction(Direction::FromA)
                .filter(|ctx| promote_pem_active(ctx) && window_open(ctx)),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 491,
                    reason: "Request Pending".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        ),
        // ── Rule 6: reject A INFO/MESSAGE during window → 488 ────────────────
        rule(
            "promote-reject-a-other-indialog",
            &[],
            Match::request()
                .methods(&["INFO", "MESSAGE"])
                .direction(Direction::FromA)
                .filter(|ctx| promote_pem_active(ctx) && window_open(ctx)),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 488,
                    reason: "Not Acceptable Here".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        ),
        // ── Rule 7: absorb A's ACK to the synthetic 200 (beats relay-ack) ────
        rule(
            "promote-absorb-a-ack",
            &[],
            Match::request()
                .method("ACK")
                .direction(Direction::FromA)
                .filter(|ctx| promote_pem_active(ctx) && promoted(ctx) && ctx.call.active_peer.is_none()),
            |_ctx| {
                ok(vec![RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Provisional,
                    leg_id: "a".to_string(),
                    status_code: Some(0),
                    reason: Some("promote-pem-to-200:ack-absorbed".to_string()),
                }])
            },
        ),
        // ── Rule 8: B fails post-promote → BYE A with Reason ─────────────────
        rule(
            "promote-b-fails-post-promote",
            &[],
            Match::response()
                .method("INVITE")
                .direction(Direction::FromB)
                .call_state(call::CallModelState::Active)
                .filter(|ctx| {
                    if !promote_pem_active(ctx) || !promoted(ctx) {
                        return false;
                    }
                    ctx.response().map(|r| r.status >= 300).unwrap_or(false)
                }),
            |ctx| {
                let resp = ctx.response()?;
                let b = ctx.source_leg_id.to_string();
                let reason = reason_header(resp.status, &resp.reason);
                ok(vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Reject,
                        leg_id: b.clone(),
                        status_code: Some(resp.status as i64),
                        reason: Some(format!("promote-pem-to-200:b-failed:{reason}")),
                    },
                    RuleAction::TerminateLeg {
                        leg_id: b,
                        bye_disposition: Some(call::ByeDisposition::Rejected),
                    },
                    RuleAction::SetPromotePem { state: None },
                    RuleAction::BeginTermination { reason: Some(reason) },
                ])
            },
        ),
    ]
}

fn keepalive_interval(ctx: &RuleContext) -> i64 {
    // Operator/worker knob — see `defaults::keepalive_interval`.
    ctx.config.keepalive_interval_sec
}
fn max_duration(ctx: &RuleContext) -> i64 {
    ctx.call
        .features
        .as_ref()
        .map(|f| f.platform.max_duration_sec)
        .unwrap_or(3600)
}

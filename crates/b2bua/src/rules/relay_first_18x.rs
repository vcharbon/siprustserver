//! `relayFirst18xTo180` — SERVICE_LAYER rules that hide forking/failover from
//! the caller. Port of `src/b2bua/rules/custom/relayFirst18xTo180.ts`.
//!
//! Activated by `features.relay_first_18x_to_180` (strategy `drop-sdp` /
//! `keep-sdp` / `fake-prack`; `promote-pem-to-200` is owned by the PEM service,
//! Slice 4). Each rule is `layer = SERVICE_LAYER` and overrides the CORE rule it
//! displaces. The rules read per-call state off `ctx.call` (the Rust rule
//! `handle`/`filter` are pure `fn` pointers — no closure ext), namely the
//! strategy (`features`), `first_relayed`/`stored_a_tag` (`relay_first_18x`),
//! and per-dialog `cached_sdp`.

use call::features::RelayFirst18xStrategy;
use call::{CdrEventType, Direction, LegState};
use sip_message::message_helpers::{get_header, get_headers};
use sip_message::SipResponse;

use super::model::{
    Match, MessageTransform, RuleAction, RuleContext, RuleDefinition, RuleHandleResult,
    SERVICE_LAYER,
};
use super::sdp_answer::{build_answer_from_offer, SdpBuildResult};

fn rule(
    id: &'static str,
    overrides: &'static [&'static str],
    matcher: Match,
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
) -> RuleDefinition {
    RuleDefinition {
        id,
        layer: SERVICE_LAYER,
        overrides,
        matcher,
        handle,
    }
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
    get_header(&resp.headers, "rseq")
        .and_then(|r| r.trim().parse::<i64>().ok())
}

fn strategy(ctx: &RuleContext) -> Option<RelayFirst18xStrategy> {
    call::helpers::relay_first_18x_strategy(ctx.call)
}

/// True when an 18x-management strategy that this module owns is active
/// (`drop-sdp` / `keep-sdp` / `fake-prack` — not `promote-pem-to-200`).
fn module_active(ctx: &RuleContext) -> bool {
    matches!(
        strategy(ctx),
        Some(RelayFirst18xStrategy::DropSdp)
            | Some(RelayFirst18xStrategy::KeepSdp)
            | Some(RelayFirst18xStrategy::FakePrack)
    )
}

fn is_fake_prack(ctx: &RuleContext) -> bool {
    strategy(ctx) == Some(RelayFirst18xStrategy::FakePrack)
}

/// The SERVICE_LAYER `relayFirst18xTo180` rules (appended to the default set;
/// dormant unless the feature activates the call).
pub fn relay_first_18x_rules() -> Vec<RuleDefinition> {
    vec![
        // ── suppress-18x (overrides relay-provisional) ───────────────────────
        rule(
            "suppress-18x",
            &["relay-provisional"],
            Match::response()
                .method("INVITE")
                .status_class(1)
                .direction(Direction::FromB)
                .filter(module_active),
            |ctx| {
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

                if call::helpers::relay_first_18x_first_relayed(ctx.call) {
                    // Subsequent 18x — suppress relay; still PRACK + cache.
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

                // First 18x — mint an a-facing tag + relay as a bare 180 (the
                // executor owns the IdGen and the tag-map seeding).
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
        ),
        // ── force-tag-consistency (composes with confirm-dialog) ─────────────
        //
        // Pre-seed the tag map with the stored a-facing tag so confirm-dialog
        // reuses it (200 OK To-tag matches the first 180), and (fake-prack)
        // stage the winning dialog's cached SDP into policy_update_body so the
        // relay substitutes it into the 200 toward alice. Does NOT override
        // confirm-dialog (both must run); ordered before it by SERVICE_LAYER so
        // the tag map / policy body are set when confirm-dialog relays.
        rule(
            "force-tag-consistency",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .filter(module_active),
            |ctx| {
                let resp = ctx.response()?;
                let b_tag = resp.to.tag.clone().unwrap_or_default();
                let leg = ctx.source_leg_id.to_string();
                let mut actions = Vec::new();

                if let Some(stored) = call::helpers::relay_first_18x_stored_a_tag(ctx.call) {
                    actions.push(RuleAction::AddTagMapping {
                        a_tag: stored.to_string(),
                        b_leg_id: leg.clone(),
                        b_tag: b_tag.clone(),
                    });
                }

                if is_fake_prack(ctx) {
                    let cached = call::helpers::cached_sdp_for_leg_dialog(ctx.call, &leg, &b_tag)
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
        ),
        // ── absorb-prack-200 (overrides relay-non-invite-200 for PRACK) ──────
        rule(
            "absorb-prack-200",
            &["relay-non-invite-200"],
            Match::response()
                .method("PRACK")
                .status_class(2)
                .direction(Direction::FromB)
                .filter(module_active),
            |ctx| {
                ok(vec![RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Provisional,
                    leg_id: ctx.source_leg_id.to_string(),
                    status_code: Some(200),
                    reason: None,
                }])
            },
        ),
        // ── fake-prack: locally answer b-leg UPDATE (overrides relay-update) ─
        rule(
            "fake-prack-handle-update-from-b",
            &["relay-update"],
            Match::request()
                .method("UPDATE")
                .direction(Direction::FromB)
                .filter(is_fake_prack),
            |ctx| {
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

                let alice_body = &ctx.call.a_leg_invite.body;
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
        ),
        // ── fake-prack: locally answer a-leg early-dialog UPDATE ─────────────
        rule(
            "fake-prack-handle-update-from-a",
            &["relay-update"],
            Match::request()
                .method("UPDATE")
                .direction(Direction::FromA)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(is_fake_prack),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 200,
                    reason: "OK".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        ),
    ]
}

/// Replay `confirm-dialog`'s action sequence (the `force-tag-consistency` rule
/// composes with it: it wins the 2xx match, so it must emit confirm-dialog's
/// effects itself). Kept in sync with `defaults.rs::confirm-dialog`.
fn confirm_dialog_actions(ctx: &RuleContext) -> Vec<RuleAction> {
    use call::TimerType;
    let b = ctx.source_leg_id.to_string();
    let a = ctx.call.a_leg.leg_id.clone();
    let max_duration = ctx
        .call
        .features
        .as_ref()
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

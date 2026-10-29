//! The CORE_LAYER rules of the bridged-call lifecycle: INVITE → 18x → 200 →
//! ACK → in-dialog → BYE, plus CANCEL, b-leg failure, failover resolution, and
//! the housekeeping timers. Rules are registered in priority order: corner
//! cases + failure resolution first (narrow matches), broad relays last;
//! `overrides` removes a displaced rule regardless of order. Which families
//! compose around this list — and in what order — is owned by
//! [`super::compose`].

use call::helpers::{PrackDisposition, RAckTokens};
use call::TxnTimeoutKind;
use call::{
    ByeDisposition, CSeq, CallModelState, CdrEventType, DialogState, Direction, LegDisposition,
    LegState, Obligation, RSeq, TerminationCause, TimeoutKind, TimerKey, TimerType,
};
use sip_message::header::RAck;
use sip_message::{Method, SipRequest};

use b2bua_sdk::model::{
    LegEnd, Match, MessageTransform, RuleAction, RuleCall, RuleContext, RuleDefinition,
    RuleHandleResult, TimerDelay, CORE_LAYER,
};
use b2bua_sdk::provisional::{
    absorbed_provisional_actions, originator_final_sent, shown_to_no_one,
};

use super::reaper_verdicts::{reap_handler_panic, reap_message_cap, reap_overflow, reap_stale};
use super::request_timeouts::{going_away_timeout, record_timeout_actions};
use super::straggler::{fork_straggler, straggler_release, straggler_release_timeout};

/// A CORE_LAYER rule.
pub(super) fn rule(
    id: &'static str,
    overrides: &'static [&'static str],
    matcher: Match,
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
) -> RuleDefinition {
    RuleDefinition::core(id, CORE_LAYER, overrides, matcher, handle)
}

pub(super) fn ok(actions: Vec<RuleAction>) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(actions))
}

fn no_transform() -> MessageTransform {
    MessageTransform::default()
}

/// What an un-ACKed 2xx's silence means (RFC 3261 §13.3.1.4): the peer that
/// owed the ACK is gone, so the session ends — `BeginTermination` BYEs every
/// confirmed leg and the → terminated invariant settles the obligations. The
/// CDR names the leg that owed the ACK under the marker of the 2xx it never
/// acknowledged: the call's own answer, or a relayed re-INVITE's. The verdict
/// of both CORE give-up rules, and the framework's when a service rule
/// re-authored the give-up without ending the session (ADR-0032 X5).
///
/// §13.2.2.4's "after acknowledging … MUST terminate with a BYE" still binds
/// the other face: a dialog whose 2xx this stack can acknowledge on its own
/// ([`still_owed_bare_ack`]) is ACKed before the BYE, so the answerer's INVITE
/// server transaction quiesces instead of laddering under a BYE it cannot match.
pub(crate) fn unacked_2xx_give_up_actions(
    call: &RuleCall,
    obligation: &Obligation,
) -> Vec<RuleAction> {
    let Obligation::AckOf2xx { leg, .. } = obligation else {
        return vec![];
    };
    let (marker, reason) = if call.answers_initial_invite(obligation) {
        ("ack_timeout", "ack-timeout")
    } else {
        ("reinvite_ack_timeout", "reinvite-ack-timeout")
    };
    let mut actions: Vec<RuleAction> =
        still_owed_bare_ack(call).map(|leg_id| RuleAction::AckLeg { leg_id, body: None }).collect();
    actions.extend([
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Bye,
            leg_id: leg.clone(),
            status_code: None,
            reason: Some(marker.into()),
        },
        RuleAction::BeginTermination {
            reason: Some(reason.into()),
            cause: TerminationCause::Timeout(TimeoutKind::Ack),
            by_leg: Some(leg.clone()),
        },
    ]);
    actions
}

/// Every leg still holding a 2xx whose ACK this stack can compose alone
/// ([`crate::rules::relay::bare_ack_owed`]). Either face qualifies — a b-leg
/// answering the call, or the caller's face answering a relayed callee
/// re-INVITE. Absent: a delayed-offer 2xx, whose ACK the teardown composes
/// rejecting every stream (RFC 3264 §6), and the a-leg's initial round, where
/// this stack sent no INVITE and owes no ACK.
pub(crate) fn still_owed_bare_ack<'a>(call: &'a RuleCall) -> impl Iterator<Item = String> + 'a {
    std::iter::once(call.a_leg())
        .chain(call.b_legs().iter())
        .filter(|leg| crate::rules::relay::bare_ack_owed(leg))
        .map(|leg| leg.leg_id.clone())
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
    std::iter::once(ctx.call.a_leg()).chain(ctx.call.b_legs().iter()).find_map(|leg| {
        leg.dialogs.iter().find_map(|d| {
            call::helpers::invite_relay(d)
                .filter(|p| p.inbound_cseq == inbound_cseq && !p.cancelled)
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
    // 32 s) — wide enough that a healthy restored dialog whose keepalive
    // round-trip is still settling across a reboot is not BYE'd as dead.
    ctx.config.keepalive_timeout_sec
}
fn max_duration(ctx: &RuleContext) -> i64 {
    ctx.call.features().map(|f| f.platform.max_duration_ms).unwrap_or(3_600_000)
}
/// The obligation of a fired ladder give-up.
fn fired_give_up<'a>(ctx: &'a RuleContext) -> Option<&'a Obligation> {
    (ctx.timer_type() == Some(&TimerType::RepeatGiveUp))
        .then(|| ctx.timer_key().and_then(TimerKey::obligation_of))
        .flatten()
}
/// The duration cap fired, or a release consult it raised went unanswered by
/// its deadline (ADR-0039): either way the release event stands.
fn release_event_due(ctx: &RuleContext) -> bool {
    matches!(ctx.timer_type(), Some(TimerType::GlobalDuration | TimerType::ReleaseAnswer { .. }))
}
/// The CORE_LAYER rule set, in registration (priority) order.
pub(super) fn core_rules() -> Vec<RuleDefinition> {
    let mut rules = vec![
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
                    RuleAction::AckLeg { leg_id: b.clone(), body: None },
                    // The crossing 200 answered the callee's dialog and the
                    // DestroyLeg below BYEs it: the CDR records both, so a
                    // b-leg reading `Confirmed`/`Bridged` at a cancelled call's
                    // terminal is explained by its own event stream rather than
                    // ending at the `Cancel` the caller's CANCEL wrote.
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Answer,
                        leg_id: b.clone(),
                        status_code: Some(200),
                        reason: Some("cancel_crossing".into()),
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Bye,
                        leg_id: b.clone(),
                        status_code: None,
                        reason: Some("cancel_crossing".into()),
                    },
                    RuleAction::DestroyLeg { leg_id: b, headers: vec![] },
                ])
            },
        ),
        // A provisional (1xx) from a callee whose leg is being CANCELLed: the
        // caller's INVITE server transaction has already completed (487 on the
        // CANCEL), so relaying it would put a new 1xx after that final — forbidden
        // (RFC 3261 §13.3.1.1 / §17.2.1). Absorbed; the engine still PRACKs a
        // reliable one, since the CANCEL does not end the INVITE transaction
        // (RFC 3262 §4). The crossing 2xx/487 is owned by `cancel-200-crossing` /
        // `resolve-cancel-response`. The 1xx sibling of `cancel-200-crossing`;
        // outranks `relay-provisional`, which is disposition-blind.
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
                ok(vec![RuleAction::TerminateLeg { leg_id: b, end: LegEnd::InviteRejected }])
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
        // Re-INVITE glare (RFC 3261 §14.1 / RFC 5407 §3.1): an INVITE arrives
        // while an INVITE transaction is still open on the dialog it arrived on
        // OR on the dialog its relay would be regenerated on — a relayed
        // re-INVITE awaiting its final (§14.1 rule 1), or a 2xx un-ACKed on
        // either face: one we sent awaiting the peer's ACK, one we took whose
        // relayed ACK has not left (§14.1 rule 2; RFC 6026 *Accepted*) →
        // refuse the newcomer. Relaying it instead would
        // reset that face's ACK obligation and strand the answerer's 2xx. The
        // peer-dialog arm keeps the B2BUA from emitting its own overtaking
        // INVITE toward a face whose prior INVITE transaction has not
        // finished. Matches INBOUND INVITEs only, so a rule originating its
        // own re-INVITE is unaffected. More specific than `relay-reinvite`
        // (no filter), so it wins on glare. The answer follows what is open
        // (`GlareRefusal`, §14.2): 500 when the sender's own INVITE has no
        // final, else 491.
        rule(
            "reinvite-glare",
            &["relay-reinvite"],
            Match::request().method("INVITE").filter(|ctx| ctx.invite_glare()),
            |ctx| ok(vec![RuleAction::RefuseGlare { refusal: ctx.glare_refusal() }]),
        ),
        // UPDATE glare (RFC 3311 §5.2, `RuleContext::update_refusal`): 500 over
        // the sender's UPDATE awaiting its final, whatever the body; an offer
        // draws 500 over the sender's open INVITE offer and 491 over this
        // stack's (`open_offer`). Refused, it is never relayed. It outranks the
        // 491 of an unready peer.
        rule(
            "update-glare",
            &["relay-update", "update-peer-unavailable"],
            Match::request().method("UPDATE").filter(|ctx| ctx.update_refusal().is_some()),
            |ctx| {
                let refusal = ctx.update_refusal()?;
                ok(vec![RuleAction::RefuseGlare { refusal }])
            },
        ),
        // In-dialog UPDATE while the peer side is NOT in a relayable state:
        // no peer leg, the peer leg terminated by a failure whose
        // `/call/failure` reroute is still pending, or a replacement leg that
        // holds no dialog yet. `relay-update` would either be silently
        // dropped by the relay machinery (no target dialog) or fired into
        // a dead dialog, leaving the requester to time out. Answer **491
        // Request Pending** locally instead (RFC 5407 §3.1 glare treatment —
        // the spec-expected "retry later" during a pending failover; the
        // requester re-UPDATEs once the reroute settles). The condition rides
        // `RuleContext::peer_relay_ready` — the same resolver the relay
        // executor uses — and is part of the rule vocabulary, so a
        // SERVICE_LAYER rule can out-rank this CORE default and own the policy
        // (park, alternate code, …). A legitimate early-dialog UPDATE
        // (RFC 3311 §5.1 — an early peer dialog) stays relayable and
        // never matches here.
        rule(
            "update-peer-unavailable",
            &["relay-update"],
            Match::request().method("UPDATE").filter(|ctx| !ctx.peer_relay_ready()),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 491,
                    reason: "Request Pending".into(),
                    body: vec![],
                    content_type: None,
                }])
            },
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
            Match::response()
                .method("INVITE")
                .filter(|ctx| ctx.relayed_pending().is_some_and(|p| p.cancelled)),
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
                    actions.push(RuleAction::AckLeg { leg_id: leg.clone(), body: None });
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
        // Also outranks `handle-481`: a relayed re-INVITE's 481 is the far
        // end's answer to relay (RFC 3261 §14.1, the session unchanged), not
        // a denial of the dialog we hold.
        rule(
            "relay-reinvite-response",
            &["relay-provisional", "confirm-dialog", "route-failure", "handle-481"],
            // A `cancelled` snapshot is NOT relayable — its originator was
            // already 487'd by the txn layer when the CANCEL matched; the final
            // resolves via `resolve-cancelled-reinvite-response`.
            Match::response()
                .method("INVITE")
                .filter(|ctx| ctx.relayed_pending().is_some_and(|p| !p.cancelled)),
            |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
        ),
        // RFC 3261 §13.2.2.4 — re-ACK a **retransmitted 2xx** whose first ACK was
        // lost. §13.2.2.4 gives one ACK per 2xx and re-passes THAT ACK to the
        // transport for every copy, so the re-ACK MUST reuse the client
        // transaction the ACK went out on (the retained `ack_branch` + the INVITE
        // CSeq) — a fresh branch would mint a new transaction and never quiesce
        // the answerer's INVITE server txn, leaking the confirmed call or timing
        // it out late under packet loss. `ack_leg` re-passes the retained
        // datagram (`emitted_ack`) raw, so whatever body the acknowledging party
        // put on it rides every repeat. This is the b-leg / realign twin of the
        // a-leg `unacked-2xx-retransmit` (which retransmits the B2BUA's *own* 2xx
        // to a silent caller).
        //
        // Fires ONLY on a genuine retransmit that nothing else claims: the source
        // dialog holds an `ack_branch` — its ACK client transaction, minted by the
        // ACK's emission and reset on every new INVITE txn — the response echoes
        // that INVITE's CSeq, and no pending-relay snapshot is open (a first-time
        // re-INVITE final is claimed by `relay-reinvite-response` / the
        // realign-200 rules, whose `ack_branch` is still `None`). A 2xx whose ACK
        // has not left yet draws nothing: no branch exists to re-send on, and the
        // answerer's own ladder is the mechanism §13.3.1.4 provides while that
        // ACK is on its way.
        // Absorbs (AckLeg only — no relay to the peer, which already saw the first
        // final).
        //
        // A leg the BYE already Terminated still owes it (RFC 5407 §2 + App. D:
        // a Mortal UA keeps the invite usage for exactly this, and §13.2.2.4's
        // "every time a retransmission arrives" does not lapse with the dialog).
        rule(
            "re-ack-retransmitted-2xx",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .leg_states(&[LegState::Confirmed, LegState::Terminated])
                .filter(|ctx| {
                    let Some(d) = ctx.dialog() else { return false };
                    let Some(leg) = ctx.source_leg() else { return false };
                    let Some(resp) = ctx.response() else { return false };
                    call::helpers::sent_exchange(d).is_some_and(|x| x.ack_branch.is_some())
                        && crate::rules::relay::retransmitted_2xx(leg, d, resp)
                }),
            |ctx| {
                ok(vec![RuleAction::AckLeg { leg_id: ctx.source_leg_id.to_string(), body: None }])
            },
        ),
        // RFC 3261 §13.2.2.4 — a fork STRAGGLER's 2xx: a 2xx to the INVITE a
        // b-leg sent, under a To-tag no live dialog of that leg carries, once another
        // fork confirmed the leg. The UAC ACKs every 2xx and BYEs a dialog it
        // does not want; the call keeps the winner. A repeat is re-ACKed alone.
        rule(
            "release-fork-straggler-2xx",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .leg_states(&[LegState::Confirmed, LegState::Terminated])
                .direction(Direction::FromB)
                .filter(fork_straggler),
            |ctx| {
                ok(vec![RuleAction::ReleaseStraggler2xx { leg_id: ctx.source_leg_id.to_string() }])
            },
        )
        // A call already ending still owes the straggler its ACK and BYE.
        .runs_while_terminating(),
        // ── dialog ──────────────────────────────────────────────────────────
        // A callee's INVITE provisional, relayed toward the originator while
        // her INVITE transaction is open. Once that transaction sent its
        // final it emits no further provisional (RFC 3261 §13.3.1.1 /
        // §17.2.1): a leg ringing then — dialled mid-call, or still ringing
        // after her CANCEL — is absorbed with what it is owed
        // (`absorbed_provisional_actions`), as is one from a leg with no
        // relay peer (`shown_to_no_one`). A provisional on a leg that already
        // took its final answers nothing and is no candidate here.
        rule(
            "relay-provisional",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(1)
                .leg_states(&[LegState::Trying, LegState::Early])
                .direction(Direction::FromB),
            |ctx| {
                if shown_to_no_one(ctx) {
                    return ok(absorbed_provisional_actions(ctx));
                }
                let b = ctx.source_leg_id.to_string();
                let status = ctx.response().map(|r| r.status() as i64);
                ok(vec![
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
                ];
                // RFC 3261 §13.2.2.4: the ACK this 2xx owes is the caller's own,
                // relayed when it arrives (`confirm_dialog` arms the obligation).
                // Nothing is composed here.
                actions.extend(vec![
                    RuleAction::cancel(TimerKey::leg(TimerType::NoAnswer, &b)),
                    RuleAction::cancel(TimerKey::setup(TimerType::SetupTimeout)),
                    RuleAction::arm(
                        TimerKey::call(TimerType::GlobalDuration),
                        TimerDelay::millis(max_duration(ctx)),
                    ),
                    RuleAction::arm(
                        TimerKey::call(TimerType::Keepalive),
                        TimerDelay::secs(keepalive_interval(ctx)),
                    ),
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
                        ext: crate::rules::relay::failure_headers_ext(
                            None,
                            ctx.call.a_leg_invite(),
                            ctx.config,
                        ),
                    },
                ]);
                // RFC 3261 §13.3.1.4: the a-leg 2xx's ladder is armed where
                // the a-facing 2xx is emitted (`actions::respond::send_a_leg_answer`),
                // not here — every path that answers the caller owes it,
                // including the service rules that replay this rule's actions.
                ok(actions)
            },
        ),
        rule(
            "relay-non-invite-200",
            &[],
            Match::response()
                .methods(&[
                    "OPTIONS",
                    "INFO",
                    "PRACK",
                    "UPDATE",
                    "REFER",
                    "MESSAGE",
                    "SUBSCRIBE",
                    "NOTIFY",
                ])
                .status_class(2),
            |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
        ),
        // A relayed non-INVITE request's **non-2xx final** is relayed back to
        // its requester — plain transaction-layer symmetry (RFC 3261 §8.1.3.3).
        // The non-INVITE sibling of `relay-reinvite-response`
        // (INVITE) alongside `relay-non-invite-200` (the 2xx half): without it
        // the far end's 481/488/491… to a relayed UPDATE/INFO was silently
        // dropped and the requester timed out. Matches ONLY when the response
        // answers a relay record on the source dialog — i.e. a transaction WE
        // relayed. A B2BUA-originated request (keepalive OPTIONS,
        // relayFirst18x PRACK, REFER-progress NOTIFY) records as this stack's
        // own, so its failures keep their own rules — in particular a
        // keepalive-OPTIONS 481 still reaches `handle-481`'s teardown, which
        // this rule outranks only for relayed transactions. Like a failed
        // re-INVITE (§14.1), a failed relayed non-INVITE leaves the dialog and
        // the call as they were: report the failure to the requester, nothing
        // more. (BYE never takes this path — it is answered locally and
        // relayed by no record; the relay executor settles the record on this
        // final.)
        rule(
            "relay-non-invite-failure",
            &["handle-481"],
            Match::response()
                .methods(&[
                    "OPTIONS",
                    "INFO",
                    "PRACK",
                    "UPDATE",
                    "REFER",
                    "MESSAGE",
                    "SUBSCRIBE",
                    "NOTIFY",
                ])
                .filter(|ctx| {
                    ctx.response().is_some_and(|r| r.status() >= 300)
                        && ctx.answers_relayed_request()
                }),
            |_ctx| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
        ),
        // ── failure ─────────────────────────────────────────────────────────
        rule(
            "route-failure",
            &[],
            Match::response()
                .method("INVITE")
                .leg_states(&[LegState::Trying, LegState::Early])
                .direction(Direction::FromB)
                .filter(|ctx| ctx.response().map(|r| r.status() >= 300).unwrap_or(false)),
            |ctx| {
                let b = ctx.source_leg_id.to_string();
                let (status, reason) = ctx
                    .response()
                    .map(|r| (r.status() as i64, r.reason().to_string()))
                    .unwrap_or((500, "Server Error".into()));
                // Record the reject; the relay/terminate (or failover) is decided next, and
                // the failed leg is torn down after it so the caller's final leaves before
                // the answers the teardown owes her (a PRACK the final crossed).
                let mut actions = vec![RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Reject,
                    leg_id: b.clone(),
                    status_code: Some(status),
                    reason: Some(reason.to_string()),
                }];
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
                        // withholds credentials, and a concealed identity when
                        // the worker is the privacy service), so
                        // the a-facing final answering the consult carries
                        // what the callee stated. It
                        // is restated on EVERY consult, so a superseded
                        // attempt's image never answers a later failure.
                        actions.push(RuleAction::MergeCallExt {
                            ext: crate::rules::relay::failure_headers_ext(
                                ctx.response(),
                                ctx.call.a_leg_invite(),
                                ctx.config,
                            ),
                        });
                        actions.push(RuleAction::TerminateLeg {
                            leg_id: b.clone(),
                            end: LegEnd::InviteRejected,
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
                        actions.push(RuleAction::TerminateLeg {
                            leg_id: b.clone(),
                            end: LegEnd::InviteRejected,
                        });
                        actions.push(RuleAction::TerminateCall {
                            cause: TerminationCause::RemoteFinal,
                            by_leg: Some(b.clone()),
                        });
                    }
                }
                ok(actions)
            },
        ),
    ];
    // ── failover resolution (/call/failure result) ──────────────────────────
    rules.extend(super::failover_rules::failover_rules());
    rules.extend([
        // A 481 to a re-INVITE, UPDATE, INFO, MESSAGE, REFER or keepalive OPTIONS
        // denies the dialog (RFC 3261 §12.2.1.2) and ends the call. It declines a
        // PRACK's (RFC 3262 §3), CANCEL's (RFC 3261 §9.2), NOTIFY's (RFC 6665 §4.4.1)
        // and BYE's (RFC 3261 §15.1.1: the dialog ends either way); a relayed one is
        // `relay-non-invite-failure`'s and the rest `absorb-own-request-failure`'s.
        //
        // The 481 leg is BYEd like its peer: §12.2.1.2 ends an INVITE dialog
        // by sending a BYE, whatever the peer answered. Its final to that BYE,
        // a 481 as likely as a 200, resolves the leg (`resolve-bye-response`).
        rule(
            "handle-481",
            &[],
            Match::response().status_code(481).call_state(CallModelState::Active).filter(|ctx| {
                ctx.response().is_some_and(|r| {
                    !matches!(
                        r.cseq().method(),
                        Method::Prack | Method::Cancel | Method::Notify | Method::Bye
                    )
                })
            }),
            |ctx| {
                let src = ctx.source_leg_id.to_string();
                // A 481 to the liveness probe is the probe's failure — the
                // peer holds no dialog to answer it in — and ends the call
                // under the keepalive deadline; to anything else it is the
                // peer's final denying the dialog.
                let cause = if ctx.response().is_some_and(|r| r.cseq().method() == Method::Options)
                {
                    TerminationCause::Timeout(TimeoutKind::Keepalive)
                } else {
                    TerminationCause::RemoteFinal
                };
                ok(vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Bye,
                        leg_id: src.clone(),
                        status_code: Some(481),
                        reason: Some("Call/Transaction Does Not Exist".into()),
                    },
                    RuleAction::BeginTermination {
                        reason: Some("481".into()),
                        cause,
                        by_leg: Some(src),
                    },
                ])
            },
        ),
        // A non-2xx final to a request THIS STACK originated — its own PRACK
        // toward a responder whose provisional the originator was never shown
        // reliably, or a REFER-progress NOTIFY — records as this stack's own,
        // and denies one transaction or one subscription, never the
        // dialog: a PRACK's 481 is RFC 3262 §3's ordinary answer once the
        // responder no longer holds that provisional unacknowledged; a
        // NOTIFY's that removes its subscription ended it before the rules
        // (RFC 6665 §4.2.2). Nothing is owed and nothing moves. A RELAYED
        // request's failure is `relay-non-invite-failure`'s to relay.
        rule(
            "absorb-own-request-failure",
            &[],
            Match::response().methods(&["PRACK", "NOTIFY"]).filter(|ctx| {
                ctx.response().is_some_and(|r| r.status() >= 300) && !ctx.answers_relayed_request()
            }),
            |_ctx| ok(vec![]),
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
            // Only a B2BUA-originated keepalive OPTIONS is absorbed: its record
            // is this stack's own. A relayed end-to-end OPTIONS's record is a
            // relay → this declines and `relay-non-invite-200` forwards the 200
            // to the peer.
            Match::response()
                .method("OPTIONS")
                .status_class(2)
                .filter(|ctx| !ctx.answers_relayed_request()),
            |ctx| {
                let leg = ctx.source_leg_id.to_string();
                ok(vec![RuleAction::cancel(TimerKey::leg(TimerType::KeepaliveTimeout, leg))])
            },
        ),
        rule(
            "absorb-notify-200",
            &["relay-non-invite-200"],
            // Only a B2BUA-originated NOTIFY is absorbed: the `referTransfer`
            // machine's `SendNotify` (refer-progress sipfrag toward the referrer)
            // is this stack's own. A **relayed** NOTIFY — the transparent-REFER
            // path forwards the transferee's implicit-subscription NOTIFYs (RFC
            // 3515) end-to-end — is a relay → this declines and
            // `relay-non-invite-200` forwards the 200 back to the requester.
            // Mirrors `absorb-options-200`.
            Match::response()
                .method("NOTIFY")
                .status_class(2)
                .filter(|ctx| !ctx.answers_relayed_request()),
            |_ctx| ok(vec![]),
        ),
        // ── fork-straggler release ──────────────────────────────────────────
        // The BYE that released a fork straggler (`release-fork-straggler-2xx`)
        // is that release's alone: its final, whatever the status, and its
        // transaction timeout are absorbed, and the winning dialog on the same
        // leg never reads them as its own.
        rule(
            "absorb-straggler-release-final",
            &[],
            Match::response().method("BYE").filter(|ctx| straggler_release(ctx.resolved)),
            |_ctx| ok(vec![]),
        )
        .runs_while_terminating(),
        // The straggler's own BYE crossing the release (RFC 3261 §15.1.2): the
        // gate passes it on the retired dialog alone, and 200 is all it
        // takes; the release's final still settles the release.
        rule(
            "straggler-crossing-bye",
            &["relay-bye", "resolve-cross-bye", "resolve-ended-leg-bye"],
            Match::request()
                .method("BYE")
                .filter(|ctx| ctx.dialog().is_some_and(|d| d.is_retired())),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 200,
                    reason: "OK".into(),
                    body: vec![],
                    content_type: None,
                }])
            },
        )
        .runs_while_terminating(),
    ]);
    rules.extend([
        // ── terminating ─────────────────────────────────────────────────────
        rule(
            "resolve-bye-response",
            &["absorb-bye-200"],
            Match::response().method("BYE").filter(|ctx| {
                ctx.source_leg()
                    .and_then(|l| l.end())
                    .map(|d| d == ByeDisposition::ByeSent)
                    .unwrap_or(false)
            }),
            |ctx| {
                ok(vec![RuleAction::TerminateLeg {
                    leg_id: ctx.source_leg_id.to_string(),
                    end: LegEnd::OurByeAnswered,
                }])
            },
        ),
        // A BYE crossing this stack's own BYE or CANCEL (RFC 3261 §15.1.2) on
        // the dialog it is ending is answered 200 and ends that dialog.
        rule(
            "resolve-cross-bye",
            &["relay-bye"],
            Match::request().method("BYE").filter(|ctx| {
                ctx.dialog().is_some_and(|d| matches!(d.state, DialogState::Ending(_)))
            }),
            |ctx| {
                ok(vec![
                    RuleAction::Respond {
                        status: 200,
                        reason: "OK".into(),
                        body: vec![],
                        content_type: None,
                    },
                    RuleAction::TerminateLeg {
                        leg_id: ctx.source_leg_id.to_string(),
                        end: LegEnd::PeerByeAnswered,
                    },
                ])
            },
        ),
        // ── relay (broad) ───────────────────────────────────────────────────
        // An ACK relays to the peer. The ladder it discharges — the a-leg's
        // initial 2xx or a re-INVITE 2xx on either face (RFC 3261 §13.3.1.4)
        // — the engine already retired before this rule ran, matched on the
        // ACK's To-tag and CSeq (`ctx.discharged`); no rule cancels a ladder.
        rule("relay-ack", &[], Match::request().method("ACK"), |_ctx| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        // A BYE on a live dialog of a leg that reads `Terminated` (its CANCEL
        // in flight, its INVITE failed, another of its dialogs ended) ends
        // that dialog alone: the leg is no longer a party of the session, so
        // its BYE is answered 200 and resolved on the leg while the call
        // stays up.
        rule(
            "resolve-ended-leg-bye",
            &["relay-bye"],
            Match::request().method("BYE").filter(|ctx| {
                ctx.source_leg().map(|l| l.state() == LegState::Terminated).unwrap_or(false)
            }),
            |ctx| {
                ok(vec![
                    RuleAction::Respond {
                        status: 200,
                        reason: "OK".into(),
                        body: vec![],
                        content_type: None,
                    },
                    RuleAction::TerminateLeg {
                        leg_id: ctx.source_leg_id.to_string(),
                        end: LegEnd::PeerByeAnswered,
                    },
                ])
            },
        ),
        // A caller's BYE on the early dialog its INVITE opened (RFC 3261 §15;
        // the dialog is the local tag a relayed provisional stamped on the
        // a-dialog, the a-leg itself reads Trying until answered) ends the
        // setup: 200 to the BYE, 487 to the still-pending INVITE (§15.1.2
        // recommends it), the ringing callee CANCELled by the termination, the
        // record a caller release. A call already terminating keeps its
        // teardown and only gets the two finals. Outranks `relay-bye`,
        // `resolve-cross-bye` and `resolve-ended-leg-bye` (the termination
        // stamps the early a-leg's BYE disposition), which answer the BYE
        // alone and leave the INVITE transaction without its final.
        rule(
            "early-dialog-bye",
            &["relay-bye", "resolve-cross-bye", "resolve-ended-leg-bye"],
            Match::request()
                .method("BYE")
                .direction(Direction::FromA)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(|ctx| ctx.call.a_leg().invite_final_sent().is_none()),
            |ctx| {
                let a = ctx.source_leg_id.to_string();
                let mut actions = vec![
                    RuleAction::Respond {
                        status: 200,
                        reason: "OK".into(),
                        body: vec![],
                        content_type: None,
                    },
                    RuleAction::RespondToALeg {
                        status: 487,
                        reason: "Request Terminated".into(),
                        header_updates: vec![],
                        contacts: vec![],
                    },
                    RuleAction::TerminateLeg { leg_id: a.clone(), end: LegEnd::PeerByeAnswered },
                ];
                if ctx.call.state() == CallModelState::Active {
                    actions.push(RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Bye,
                        leg_id: a.clone(),
                        status_code: None,
                        reason: None,
                    });
                    actions.push(RuleAction::BeginTermination {
                        reason: Some("BYE".into()),
                        cause: TerminationCause::RemoteBye,
                        by_leg: Some(a),
                    });
                }
                ok(actions)
            },
        ),
        // A BYE from a live party of an Active call ends the session; one on a
        // leg this stack already ended is `resolve-ended-leg-bye`'s, one
        // crossing this stack's own BYE `resolve-cross-bye`'s.
        rule(
            "relay-bye",
            &[],
            Match::request().method("BYE").call_state(CallModelState::Active),
            |ctx| {
                // Pre-mark the BYE-sending leg `bye_received` (RFC 3261 §15.1.2) so the
                // subsequent begin-termination skips it (no duplicate BYE back to the
                // sender) and only tears down the peer.
                ok(vec![
                    RuleAction::Respond {
                        status: 200,
                        reason: "OK".into(),
                        body: vec![],
                        content_type: None,
                    },
                    RuleAction::TerminateLeg {
                        leg_id: ctx.source_leg_id.to_string(),
                        end: LegEnd::PeerByeAnswered,
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Bye,
                        leg_id: ctx.source_leg_id.to_string(),
                        status_code: None,
                        reason: None,
                    },
                    RuleAction::BeginTermination {
                        reason: Some("BYE".into()),
                        cause: TerminationCause::RemoteBye,
                        by_leg: Some(ctx.source_leg_id.to_string()),
                    },
                ])
            },
        ),
        rule("relay-reinvite", &[], Match::request().method("INVITE"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        // A PRACK is relayed, answered 200 or refused 481 as what it names
        // decides (`call::helpers::prack_disposition`, RFC 3262 §3): this
        // stack is the UAS of the face the provisional was shown on. One
        // carrying no readable `RAck` is refused 400 (RFC 3261 §21.4.1).
        rule("relay-prack", &[], Match::request().method("PRACK"), |ctx| {
            let respond = |status: u16, reason: &str| {
                ok(vec![RuleAction::Respond {
                    status,
                    reason: reason.into(),
                    body: vec![],
                    content_type: None,
                }])
            };
            let Some(tokens) = ctx.request().and_then(rack_tokens) else {
                return respond(400, "Bad Request");
            };
            let tag = ctx.dialog().map_or("", |d| d.our_tag.as_str());
            match ctx.call.prack_disposition(tag, tokens, ctx.discharged) {
                PrackDisposition::Relay => {
                    ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
                }
                PrackDisposition::Answer200 => respond(200, "OK"),
                PrackDisposition::Refuse481 => respond(481, "Call/Transaction Does Not Exist"),
            }
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
        // Transparent in-dialog REFER relay: forward a REFER to the peer leg
        // like INFO/MESSAGE — the DEFAULT for any call whose routing decision did
        // not activate local REFER processing (`features.refer`), Refer-To
        // readable or not. On a call that did activate it, the `refer_transfer`
        // seed (also CORE, registered earlier) out-ranks this and terminates the
        // REFER instead. The 202/failure finals ride `relay-non-invite-200` /
        // `relay-non-invite-failure` (REFER is in both method sets), and the
        // transferee's implicit-subscription NOTIFYs ride `relay-notify` — the
        // whole RFC 3515 exchange passes through.
        rule("relay-refer", &[], Match::request().method("REFER"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        // Transparent in-dialog NOTIFY relay: forward an in-dialog NOTIFY request
        // to the peer leg (the implicit REFER subscription's progress reports ride
        // the dialog end-to-end on the transparent-transfer path). Its 200/failure
        // finals relay back via `relay-non-invite-200`/`-failure` (NOTIFY is in both
        // sets); `absorb-notify-200`'s record filter keeps a B2BUA-*originated*
        // NOTIFY's 200 absorbed. A SERVICE_LAYER rule (e.g. a subscribed transfer
        // machine that owns NOTIFY) out-ranks this CORE default when active.
        rule("relay-notify", &[], Match::request().method("NOTIFY"), |_| {
            ok(vec![RuleAction::RelayToPeer { transform: no_transform() }])
        }),
        // An in-dialog request of a method this stack models no relay for (a
        // SUBSCRIBE, a PUBLISH, a REGISTER, an extension method) goes to the
        // relay too, which answers it locally (489 to a SUBSCRIBE, else 405):
        // no request is left to run out its sender's Timer F (RFC 3261 §8.2).
        rule(
            "relay-unmodelled",
            &[],
            Match::request().filter(|ctx| ctx.request().is_some_and(|r| unmodelled(r.method()))),
            |_| ok(vec![RuleAction::RelayToPeer { transform: no_transform() }]),
        ),
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
                    RuleAction::CancelPendingReinvite { leg_id: leg_id.clone(), outbound_cseq },
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
                match b.state() {
                    LegState::Confirmed => actions
                        .push(RuleAction::DestroyLeg { leg_id: b.leg_id.clone(), headers: vec![] }),
                    LegState::Trying | LegState::Early => {
                        actions.push(RuleAction::CancelLeg { leg_id: b.leg_id.clone() })
                    }
                    LegState::Terminated => {}
                }
            }
            actions.push(RuleAction::AddCdrEvent {
                event_type: CdrEventType::Cancel,
                leg_id: ctx.call.a_leg().leg_id.clone(),
                status_code: None,
                reason: None,
            });
            actions.push(RuleAction::BeginTermination {
                reason: Some("CANCEL".into()),
                cause: TerminationCause::RemoteCancel,
                by_leg: Some(ctx.call.a_leg().leg_id.clone()),
            });
            ok(actions)
        }),
        rule("handle-timeout", &[], Match::timeout(), |ctx| {
            // The BYE that released a fork straggler times out the release's
            // alone (§15.1.1): the winning dialog on the leg never reads it.
            if straggler_release_timeout(ctx) {
                return ok(vec![]);
            }
            // A request record's timeout reads by the record's origin and the
            // method table (ADR-0055 §2).
            if let Some(record) = ctx.timed_out_record() {
                return ok(record_timeout_actions(ctx, record));
            }
            // A **pending b-leg INVITE** transaction timeout (Timer B / the long
            // INVITE backstop) on a failover-capable call is a failure the
            // decision backend must get a shot at — the dead-gateway reroute is
            // the classic failover case. Mirror the
            // `no-answer` shape: record the timeout, destroy the failed leg
            // (its CANCEL meets the transaction still open: sent if the callee
            // answered a provisional, held for one otherwise, §9.1), and let
            // `call-failure-result` drive the outcome.
            // Everything else (a-leg, confirmed-leg re-INVITE, BYE/OPTIONS
            // timeouts, no callback context) keeps the unconditional
            // termination below.
            let timed_out_invite = ctx.timeout_method().map(|m| m == "INVITE").unwrap_or(false);
            let pending_b_leg = ctx.source_leg().is_some_and(|l| {
                l.leg_id != ctx.call.a_leg().leg_id
                    && matches!(l.state(), LegState::Trying | LegState::Early)
            });
            // A leg already going away (caller-CANCELed → `Cancelling`, its own
            // state `Terminated`, or the whole call `Terminating`) makes no
            // forward progress on its transaction timeout: no /call/failure
            // consult, and above all no `BeginTermination`, which re-schedules
            // `TerminatingTimeout` and slides the safety backstop a further
            // `TERMINATING_TIMEOUT_MS` out on a call that is already tearing
            // down. The dead transaction resolves the leg locally instead
            // (`TerminateLeg` clears `Cancelling`), letting the deferred
            // termination finalize on the deadline it already has.
            if let Some(leg) = ctx.source_leg() {
                if call::helpers::leg_is_going_away(ctx.call.state(), leg) {
                    return ok(going_away_timeout(ctx, leg));
                }
            }
            if timed_out_invite && pending_b_leg {
                if let Some(cbctx) = ctx.call.callback_context() {
                    let leg = ctx.source_leg_id.to_string();
                    // Which timeout fired rides beside the origin so the
                    // decision service can route a dead hop ("response":
                    // nothing answered) apart from a callee that rang and
                    // went silent ("transaction").
                    let timeout_kind = match ctx.timeout_kind() {
                        Some(TxnTimeoutKind::Response) => "response",
                        Some(TxnTimeoutKind::Transaction) | None => "transaction",
                    };
                    return ok(vec![
                        RuleAction::AddCdrEvent {
                            event_type: CdrEventType::Timeout,
                            leg_id: leg.clone(),
                            status_code: None,
                            reason: Some("transaction_timeout".into()),
                        },
                        RuleAction::DestroyLeg { leg_id: leg.clone(), headers: vec![] },
                        // A blackholed hop drew no final: this consult states an
                        // empty relayable image, so an earlier attempt's headers
                        // cannot answer it.
                        RuleAction::MergeCallExt {
                            ext: crate::rules::relay::failure_headers_ext(
                                None,
                                ctx.call.a_leg_invite(),
                                ctx.config,
                            ),
                        },
                        RuleAction::FailureAsyncHttp {
                            request: serde_json::json!({
                                "callback_context": cbctx,
                                "origin": "transaction_timeout",
                                "timeout_kind": timeout_kind,
                                "failed_leg_id": leg,
                            }),
                        },
                    ]);
                }
            }
            // A caller whose callee's INVITE gave up is answered in this
            // turn, before the CANCELed leg resolves (RFC 3261 §9.1 lets that
            // wait 64·T1), as the failure consult's let-stand path answers it.
            let mut actions = Vec::new();
            if timed_out_invite && pending_b_leg && !originator_final_sent(&ctx.call) {
                actions.push(super::failover_rules::timeout_final(
                    ctx.timeout_kind() == Some(TxnTimeoutKind::Response),
                ));
            }
            actions.push(RuleAction::BeginTermination {
                reason: Some("timeout".into()),
                cause: TerminationCause::Timeout(TimeoutKind::Transaction),
                by_leg: Some(ctx.source_leg_id.to_string()),
            });
            ok(actions)
        })
        // Teardown rule: on a terminating call the dead transaction still
        // resolves its leg.
        .runs_while_terminating(),
        // ── timers ──────────────────────────────────────────────────────────
        // The three setup/duration deadlines are teardown rules: on a
        // terminating call they run to scrub their spent ledger entry.
        rule("no-answer", &[], Match::timer().timer_type(TimerType::NoAnswer), |ctx| {
            // NoAnswer is armed PER B-LEG at its first 18x and retired with
            // it; the router delivers only the instance the ledger holds. The fire stands
            // unless the CALLER is answered: the a-leg's INVITE transaction
            // took its 2xx (from a sibling of X), so no final may be authored
            // on it (RFC 3261 §17.2.1). A sibling b-leg's `Confirmed` alone is
            // not that: a media leg may answer while the callee still rings,
            // and its fire must stand.
            if ctx.call.a_leg().state() == LegState::Confirmed {
                return ok(vec![]);
            }
            let leg = ctx.source_leg_id.to_string();
            let mut actions = vec![
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: leg.clone(),
                    status_code: None,
                    reason: Some("no_answer_timeout".into()),
                },
                RuleAction::DestroyLeg { leg_id: leg.clone(), headers: vec![] },
            ];
            match ctx.call.callback_context() {
                // Failover-capable → ask /call/failure (origin no_answer_timeout);
                // the result drives `failover-create-leg` / `failover-terminate`.
                // A ring-forever hop drew no final, so this consult states an
                // EMPTY relayable image: the final it authors speaks for a peer
                // that never answered, not for an earlier attempt that did.
                Some(cbctx) => actions.extend([
                    RuleAction::MergeCallExt {
                        ext: crate::rules::relay::failure_headers_ext(
                            None,
                            ctx.call.a_leg_invite(),
                            ctx.config,
                        ),
                    },
                    RuleAction::FailureAsyncHttp {
                        request: serde_json::json!({
                            "callback_context": cbctx,
                            "origin": "no_answer_timeout",
                            "failed_leg_id": leg,
                        }),
                    },
                ]),
                // No consult: the caller's 480 is this turn's, under the
                // termination record's cut, then the teardown.
                None => actions.extend([
                    RuleAction::RespondToALeg {
                        status: 480,
                        reason: "Temporarily Unavailable".into(),
                        header_updates: vec![],
                        contacts: vec![],
                    },
                    RuleAction::BeginTermination {
                        reason: Some("no-answer".into()),
                        cause: TerminationCause::Timeout(TimeoutKind::NoAnswer),
                        by_leg: Some(leg),
                    },
                ]),
            }
            ok(actions)
        })
        .runs_while_terminating(),
        // The call's setup deadline (setup scope: a fire once a leg holds a
        // confirmed dialog or the call is terminating reaches no rule). Deliberately NOT per-b-leg:
        // reroute/failover creates fresh b-legs (each with its own optional
        // NoAnswer), while this caps the caller's TOTAL wait for a final
        // response, so a stuck-in-setup call is torn down at the deadline
        // instead of holding its limiter slots until GlobalDuration.
        rule("setup-timeout", &[], Match::timer().timer_type(TimerType::SetupTimeout), |ctx| {
            ok(vec![
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: ctx.call.a_leg().leg_id.clone(),
                    status_code: Some(408),
                    reason: Some("setup_timeout".into()),
                },
                // Final answer to the caller; BeginTermination then CANCELs the
                // pending (trying/early) b-legs and the → terminated invariant
                // settles the obligations (the limiter release + the CDR).
                RuleAction::RespondToALeg {
                    status: 408,
                    reason: "Request Timeout".into(),
                    header_updates: vec![],
                    contacts: vec![],
                },
                RuleAction::BeginTermination {
                    reason: Some("setup-timeout".into()),
                    cause: TerminationCause::Timeout(TimeoutKind::Setup),
                    by_leg: None,
                },
            ])
        }),
        rule("max-duration", &[], Match::timer().filter(release_event_due), |ctx| {
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
            // A release consult's deadline outlives the start of the
            // termination (its answer still settles what the consult holds),
            // so its expiry can reach a call already going away: the release
            // event is spent there — no `call_release` consult (a `reroute`
            // outcome would dial a fresh b-leg on a terminating call) and no
            // BeginTermination re-arm of the safety timer.
            if matches!(ctx.call.state(), CallModelState::Terminating | CallModelState::Terminated)
            {
                return ok(vec![]);
            }
            let answered = ctx.call.a_leg().state() == LegState::Confirmed;
            if answered
                && !ctx.call.reroute_active()
                && ctx.call.subscribed(call::ReleaseEventKind::MaxCallDuration)
            {
                if let Some(cbctx) = ctx.call.callback_context() {
                    // The consult's answer deadline is armed with it
                    // (`answer_deadline::arm`): wherever the call is served
                    // when it expires, the release event stands again and
                    // the consult is re-sent (ADR-0039).
                    return ok(vec![RuleAction::ReleaseAsyncHttp {
                        request: serde_json::json!({
                            "callback_context": cbctx,
                            "event": "max_call_duration",
                        }),
                    }]);
                }
            }
            ok(vec![
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Bye,
                    leg_id: ctx.call.a_leg().leg_id.clone(),
                    status_code: None,
                    reason: Some("max_duration".into()),
                },
                RuleAction::BeginTermination {
                    reason: Some("max-duration".into()),
                    cause: TerminationCause::MaxDuration,
                    by_leg: None,
                },
            ])
        })
        .runs_while_terminating(),
        rule(
            call::KEEPALIVE_AUTHOR,
            &[],
            Match::timer().timer_type(TimerType::Keepalive).call_state(CallModelState::Active),
            |ctx| {
                let mut actions = Vec::new();
                for leg_id in ctx.call.all_peered_legs() {
                    actions.push(RuleAction::SendRequestToLeg {
                        leg_id: leg_id.clone(),
                        method: "OPTIONS".into(),
                        body: None,
                        headers: vec![],
                    });
                    actions.push(RuleAction::arm(
                        TimerKey::leg(TimerType::KeepaliveTimeout, leg_id),
                        TimerDelay::secs(keepalive_timeout(ctx)),
                    ));
                }
                actions.push(RuleAction::arm(
                    TimerKey::call(TimerType::Keepalive),
                    TimerDelay::secs(keepalive_interval(ctx)),
                ));
                ok(actions)
            },
        ),
        // The liveness probe drew no answer within its deadline: the call ends,
        // and the silent leg is BYEd like its peer (RFC 3261 §12.2.1.2: no
        // response to an in-dialog request ends an INVITE dialog by a BYE —
        // the probe may be what was lost, not the peer). A leg silent on its
        // BYE too resolves by that BYE's transaction timeout (`handle-timeout`).
        rule(
            "keepalive-timeout",
            &[],
            Match::timer()
                .timer_type(TimerType::KeepaliveTimeout)
                .call_state(CallModelState::Active),
            |ctx| {
                ok(vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Bye,
                        leg_id: ctx.source_leg_id.to_string(),
                        status_code: None,
                        reason: Some("keepalive timeout".into()),
                    },
                    RuleAction::BeginTermination {
                        reason: Some("keepalive-timeout".into()),
                        cause: TerminationCause::Timeout(TimeoutKind::Keepalive),
                        by_leg: Some(ctx.source_leg_id.to_string()),
                    },
                ])
            },
        ),
        // ── ladder give-ups (ADR-0032 X4/X5) ─────────────────────────────────
        // The one ladder event a rule sees is a 2xx's `RepeatGiveUp
        // { obligation }`, its timers already scrubbed; these CORE rules say
        // what the silence means and a service may re-author them, of the
        // teardown's shape only: the router ends a session the rules left
        // Active (`settle_give_up`). A reliable provisional's ladder arms no
        // give-up (`arm_reliable_provisional_ladder`).
        //
        // RFC 3261 §13.3.1.4 — no ACK for the a-leg's INITIAL 2xx by the
        // deadline: the caller is gone.
        rule(
            "unacked-2xx-give-up",
            &[],
            Match::timer().call_state(CallModelState::Active).filter(|ctx| {
                matches!(
                    fired_give_up(ctx),
                    Some(o @ Obligation::AckOf2xx { .. }) if ctx.call.answers_initial_invite(o)
                )
            }),
            |ctx| {
                let Some(obligation) = fired_give_up(ctx) else {
                    return ok(vec![]);
                };
                ok(unacked_2xx_give_up_actions(&ctx.call, obligation))
            },
        ),
        // RFC 3261 §13.3.1.4 (in-dialog) — the give-up deadline elapsed with no
        // originator ACK for a relayed **re-INVITE** 2xx: a permanently-lost
        // re-INVITE ACK must never retransmit forever. The re-INVITE twin of
        // `unacked-2xx-give-up`; the CDR names the leg that owed the ACK.
        rule(
            "unacked-reinvite-2xx-give-up",
            &[],
            Match::timer().call_state(CallModelState::Active).filter(|ctx| {
                matches!(
                    fired_give_up(ctx),
                    Some(o @ Obligation::AckOf2xx { .. }) if !ctx.call.answers_initial_invite(o)
                )
            }),
            |ctx| {
                let Some(obligation) = fired_give_up(ctx) else {
                    return ok(vec![]);
                };
                ok(unacked_2xx_give_up_actions(&ctx.call, obligation))
            },
        ),
        // ── call reaper verdicts (ADR-0020 X1/X6) ───────────────────────────
        // The reaper's verdicts arrive as ordinary InternalEvents and are
        // handled by ordinary CORE rules — the single funnel. `stale` forces
        // every unresolved leg terminal with no wire traffic (the call is
        // provably dead: its stamp froze past the idle threshold) and runs
        // while terminating: a wedged terminating call is exactly what it
        // reaps. `overflow` tears the live legs down on the wire and leaves a
        // terminating call to its teardown. `fatal-error` (a handler panicked,
        // the legs are alive) and `message-cap` (the work bound, in every
        // state) do either, as the call's state asks: an Active call is torn
        // down on the wire, a terminating one forced terminal. Each records
        // the reason and begins termination; the
        // invariant then promotes → Terminated and discharges the obligations
        // (CDR + limiter) exactly once. The `discharge` outcome deliberately
        // has NO rule (the router's bypass branch owns it — rules are the
        // thing that failed by then).
        rule(
            "reaper-stale",
            &[],
            Match::internal_event()
                .topic(crate::reaper_verdict::REAPER_TOPIC)
                .outcome(crate::reaper_verdict::OUTCOME_STALE),
            reap_stale,
        )
        .runs_while_terminating(),
        rule(
            "reaper-fatal-error",
            &[],
            Match::internal_event()
                .topic(crate::reaper_verdict::REAPER_TOPIC)
                .outcome(crate::reaper_verdict::OUTCOME_FATAL),
            reap_handler_panic,
        )
        .runs_while_terminating(),
        rule(
            "reaper-overflow",
            &[],
            Match::internal_event()
                .topic(crate::reaper_verdict::REAPER_TOPIC)
                .outcome(crate::reaper_verdict::OUTCOME_OVERFLOW),
            reap_overflow,
        ),
        rule(
            "reaper-message-cap",
            &[],
            Match::internal_event()
                .topic(crate::reaper_verdict::REAPER_TOPIC)
                .outcome(crate::reaper_verdict::OUTCOME_MESSAGE_CAP),
            reap_message_cap,
        )
        .runs_while_terminating(),
        rule(
            "terminating-safety-timeout",
            &[],
            Match::timer()
                .timer_type(TimerType::TerminatingTimeout)
                .call_state(CallModelState::Terminating),
            |ctx| {
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
                            end: LegEnd::Abandon,
                        });
                    }
                }
                ok(actions)
            },
        )
        .runs_while_terminating(),
    ]);
    rules
}

/// Whether `method` is one no relay rule models: neither a method the
/// in-dialog relay builds nor an ACK or CANCEL, which have their own.
fn unmodelled(method: &Method) -> bool {
    !matches!(method, Method::Ack | Method::Cancel)
        && sip_message::generators::InDialogMethod::try_from(method).is_err()
}

/// The three `RAck` tokens a PRACK carries (RFC 3262 §7.2), or `None` when it
/// carries no readable `RAck`.
fn rack_tokens(req: &SipRequest) -> Option<RAckTokens> {
    let rack = req.header::<RAck>().and_then(Result::ok)?;
    Some(RAckTokens {
        rseq: RSeq(rack.rseq()),
        cseq: CSeq(rack.seq()),
        names_invite: *rack.method() == Method::Invite,
    })
}

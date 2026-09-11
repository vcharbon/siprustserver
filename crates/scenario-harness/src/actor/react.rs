//! The reactive answer policy — dispatch whatever arrives, whenever it
//! arrives: the inbound-request answer table (re-INVITE→200+SDP, NOTIFY→200,
//! BYE→200+terminate, CANCEL→200+487, ACK→absorb) and the CANCEL automatic.
//! Response folding does NOT live here — see [`super::response`]; the initial-
//! INVITE disposition entry lives in [`super::answer`].

use tokio::time::Instant;

use super::accept_delta::try_accept_request_delta;
use super::answer::{
    answer_initial_invite, apply_disposition, arm_reject_final, discharge_on_teardown,
    respond_200_sdp,
};
use super::endpoint::{Disposition, SUBFLOW_REALIGN};
use super::ledger::{ObligationKey, ObligationKind};
use super::response::react_response;
use super::runner::{ActorState, ParkedRequest};
use super::script::{scripted_wants_cancel, scripted_wants_in_dialog};
use super::state::{Observation, SubflowState};
use crate::{Inbound, ServerTxn, StepError};

/// The CANCEL automatic's 487 half (RFC 3261 §9.2), shared by the live CANCEL
/// arm (right after its 200) and the requeue of a stale parked CANCEL (whose
/// 200 went out at park time): take the pending initial-INVITE target — a
/// disposition hold (timed / reliable / silent), the script-BOUND initial
/// INVITE, or the parked initial — answer `487 Request Terminated` on it and
/// terminate the leg. When the target was script-claimed (bound or parked),
/// the tombstone makes a later scripted step bound to it fail fast, never by
/// goal timeout. No-op when nothing is pending (the leg already answered — a
/// late CANCEL "has no effect on the call", §9.2). `record_stray: false` on the
/// accepted-delta path: the acceptance's own `AcceptedDelta` entry is the
/// record (a blessed substitution must not double-book as divergence).
pub(super) async fn cancel_pending_initial(
    st: &mut ActorState<'_>,
    now: Instant,
    record_stray: bool,
) -> Result<(), StepError> {
    let mut consumed: Option<&'static str> = None;
    let held = st
        .pending_answer
        .take()
        .map(|ta| ta.uas)
        .or_else(|| st.pending_prack_answer.take())
        .or_else(|| st.held_silent.take())
        // A script-BOUND initial INVITE (claimed by `ExpectRequest{Initial}`,
        // provisionals already sent on the bound txn) with no scripted CANCEL
        // claim: the automatic mirrors the parked path so the peer never
        // wedges in INVITE retransmits. A bound IN-DIALOG request is not the
        // CANCEL's target and stays bound.
        .or_else(|| {
            let is_initial = st.bound.as_ref().is_some_and(|t| {
                t.request().method().as_str() == "INVITE" && t.request().to().tag().is_none()
            });
            if !is_initial {
                return None;
            }
            consumed = Some("200 + 487 on the bound INVITE");
            st.bound.take()
        })
        .or_else(|| {
            let i = st.parked.iter().position(|p| p.initial)?;
            consumed = Some("200 + 487 on the parked INVITE");
            Some(st.parked.remove(i).txn)
        });
    if let Some(mut inv) = held {
        inv.respond(487, "Request Terminated").try_send().await?;
        arm_reject_final(st, &inv, 487);
        if let Some(action) = consumed {
            st.parked_initial_consumed = Some("CANCEL answered 200 + 487");
            if record_stray {
                st.obs.record(
                    Observation::ServicedStray {
                        leg: st.role,
                        method: "CANCEL".to_string(),
                        action,
                    },
                    now,
                );
            }
        }
        st.obs.record(Observation::LegTerminated { leg: st.role }, now);
        st.scope.mark_terminated();
    }
    Ok(())
}

/// The reactive answer policy — dispatch one inbound message. Extracted and
/// generalized from the answer table in
/// [`Agent::try_receive_tolerating_blocking`]: react to WHATEVER arrives (rather
/// than "expect X, tolerate the rest"), fold the observation, and NEVER emit a
/// bodyless 200 to an offer (RFC 3264 §5).
pub(super) async fn default_react(st: &mut ActorState<'_>, msg: Inbound) -> Result<(), StepError> {
    match msg {
        Inbound::Request(txn) => react_request(st, txn).await,
        Inbound::Response(resp) => react_response(st, resp).await,
    }
}

async fn react_request(st: &mut ActorState<'_>, uas: ServerTxn) -> Result<(), StepError> {
    let method = uas.request().method().as_str().to_string();
    let is_initial_invite = method == "INVITE" && uas.request().to().tag().is_none();

    if is_initial_invite {
        return apply_disposition(st, uas).await;
    }

    // Scripted CANCEL park-or-react (ADR-0024): the CANCEL hop's `200` stays
    // a stack automatic, but the CANCEL itself PARKS when a remaining
    // `ExpectRequest{Cancel}` claims it — the script sequences the 487 on the
    // bound INVITE. The expectation always wins over the automatic; without
    // one, the automatic (200 + 487) fires in `react_in_dialog_request`
    // below. A retransmit is absorbed by its 200 alone — one parks at most.
    if matches!(st.disposition, Disposition::Scripted)
        && method == "CANCEL"
        && scripted_wants_cancel(st)
    {
        let mut uas = uas;
        uas.respond(200, "OK").try_send().await?;
        if !st.parked.iter().any(|p| p.txn.request().method().as_str() == "CANCEL") {
            st.parked.push(ParkedRequest { txn: uas, initial: false });
        }
        return Ok(());
    }

    // A Scripted actor's in-dialog park-or-react (ACK stays a stack automatic,
    // never parked; CANCEL parks only via its dedicated claim above): a request
    // a remaining `ExpectRequest` matches waits for the script; an unclaimed
    // one is offered to the plan's accepted-delta policy (ADR-0024 §6) and,
    // not accepted, falls through to the reactive core, recorded as a serviced
    // stray so divergence is never silent.
    if matches!(st.disposition, Disposition::Scripted) && method != "ACK" {
        let now = Instant::now();
        if method != "CANCEL" && scripted_wants_in_dialog(st, &method) {
            st.obs.record(
                Observation::InDialogRequest {
                    leg: st.role,
                    call_id: uas.request().call_id().to_string(),
                    cseq: uas.request().cseq().seq(),
                    method,
                },
                now,
            );
            st.parked.push(ParkedRequest { txn: uas, initial: false });
            return Ok(());
        }
        let Some(uas) = try_accept_request_delta(st, uas).await? else {
            return Ok(());
        };
        // The unclaimed CANCEL keeps its dedicated automatic (200 + 487 in the
        // reactive core) — never a plain stray record.
        if method != "CANCEL" {
            st.obs.record(
                Observation::ServicedStray {
                    leg: st.role,
                    method: method.clone(),
                    action: "auto-reacted",
                },
                now,
            );
        }
        return react_in_dialog_request(st, uas).await;
    }
    react_in_dialog_request(st, uas).await
}

/// The reactive in-dialog answer table — shared by the live dispatch and the
/// requeue-on-advance auto-react (a re-recorded `InDialogRequest` observation
/// is idempotent, so re-entry for a previously parked request is harmless).
pub(super) async fn react_in_dialog_request(
    st: &mut ActorState<'_>,
    mut uas: ServerTxn,
) -> Result<(), StepError> {
    let method = uas.request().method().as_str().to_string();
    let call_id = uas.request().call_id().clone();
    let cseq = uas.request().cseq().seq();
    let now = Instant::now();

    match method.as_str() {
        // An ACK completes a transaction — absorbed, never answered. It confirms
        // our UAS dialog (the peer ACKed our 2xx) and closes the matching
        // answered-awaiting-ACK obligation (the ACK's CSeq equals the INVITE's,
        // §13.2.2.4 — closing a never-opened key is a harmless no-op).
        "ACK" => {
            // An ACK that FOLLOWS a non-2xx reject (this leg already Terminated)
            // just completes that transaction on the wire — absorb it without
            // confirming the leg or stamping the `ack` anchor (that anchor is the
            // winning leg's; the rejected b-leg's reject-ACK must not claim it).
            // It DOES close whichever acknowledgement obligation it satisfies
            // (a reject-final's hop-ACK that surfaced unclaimed, or a recovered
            // answer-ACK landing after the leg was BYE-terminated) — closing a
            // never-opened key is a harmless no-op.
            let already_terminated = st
                .obs
                .with_snapshot(|s| s.leg(st.role).phase() == super::state::LegPhase::Terminated);
            if already_terminated {
                st.obs.record(
                    Observation::ResponseObserved {
                        key: ObligationKey::new(st.role, ObligationKind::RejectFinal, cseq),
                    },
                    now,
                );
                st.obs.record(
                    Observation::ResponseObserved {
                        key: ObligationKey::new(st.role, ObligationKind::ReInvite, cseq),
                    },
                    now,
                );
                if st.pending_reject_ack.as_ref().is_some_and(|p| p.key.cseq == cseq) {
                    st.pending_reject_ack = None;
                }
                return Ok(());
            }
            st.obs.record(
                Observation::ResponseObserved {
                    key: ObligationKey::new(st.role, ObligationKind::ReInvite, cseq),
                },
                now,
            );
            if st.answered_reinvites.remove(&cseq) {
                // The ACK to a realign re-INVITE we answered — the sub-flow the
                // refer `merged` barrier conjuncts over is confirmed.
                st.obs.record(
                    Observation::Subflow {
                        leg: st.role,
                        name: SUBFLOW_REALIGN,
                        to: SubflowState::Confirmed,
                    },
                    now,
                );
            } else {
                st.obs.record(Observation::LegConfirmed { leg: st.role }, now);
                st.ctx.anchor(&st.agent, "ack", uas.request());
                st.feed.on_ack_rx.stamp(st.ctx);
            }
        }
        // A BYE tears this leg down — UNLESS it is addressed to a LOSING fork's
        // tag (C1/E3: the caller ACK+BYEs a losing fork's late 200, §13.2.2.4):
        // that BYE ends only the abandoned early fork, the winning dialog lives
        // on, so it is 200'd (and its CSeq folded into the dialog stream) but
        // the leg is NOT terminated.
        "BYE" => {
            let is_fork_teardown =
                uas.request().to().tag().is_some_and(|t| st.fork_loser_tags.contains(t));
            st.ctx.anchor(&st.agent, "bye", uas.request());
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(
                Observation::InDialogRequest {
                    leg: st.role,
                    call_id: call_id.to_string(),
                    cseq,
                    method: method.clone(),
                },
                now,
            );
            if is_fork_teardown {
                return Ok(());
            }
            // An in-dialog ack this leg still awaits (a re-INVITE it answered, a
            // PRACK/UPDATE, …) is moot now the dialog ends (§15) — discharge it so
            // settle does not wait 32 s for an ack the torn-down peer can never send.
            discharge_on_teardown(st, now);
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
        // A CANCEL (RFC 3261 §9.2): always 200 the CANCEL. If the INVITE is
        // still PENDING (a ring not yet answered, or a reliable 183 held for its
        // PRACK), 487 the retained INVITE txn (else the peer waits Timer C and
        // reaping hangs) and terminate the leg. But if we have ALREADY answered
        // — the 200 crossed the CANCEL (C2/E5) — the CANCEL "has no effect on the
        // call" (§9.2): 200 it and IGNORE it, leaving the confirmed dialog up so
        // the caller ACKs the 200 and BYEs. NEVER terminate an already-confirmed
        // leg on a late CANCEL.
        "CANCEL" => {
            uas.respond(200, "OK").try_send().await?;
            cancel_pending_initial(st, now, true).await?;
        }
        // In-dialog non-offer requests: 200 and fold the CSeq into the dialog's
        // gap detector (all methods share the dialog CSeq space, §12.2.1.1).
        "NOTIFY" | "OPTIONS" | "INFO" | "MESSAGE" => {
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(
                Observation::InDialogRequest {
                    leg: st.role,
                    call_id: call_id.to_string(),
                    cseq,
                    method: method.clone(),
                },
                now,
            );
        }
        // An in-dialog (re-)INVITE — an offer realign. Answer 200 WITH SDP; a
        // delayed-offer bodyless re-INVITE still gets 200 + our SDP (no
        // bodyless-200 fallthrough, RFC 3264 §5). The 200 opens an
        // answered-awaiting-ACK obligation (settle holds until the peer's ACK
        // lands — the endurance failure this harness exists to expose) and
        // advances this leg's realign sub-flow; the matching ACK confirms it.
        "INVITE" => {
            st.ctx.anchor(&st.agent, "reInvite", uas.request());
            // GLARE (C4/S5+S6, RFC 3261 §14.1 / RFC 3311 §5.2): if THIS leg has
            // its OWN offer outstanding when the peer's re-INVITE arrives — an
            // un-answered re-INVITE (S5) OR an un-answered UPDATE (S6) — reject
            // the peer's with `491 Request Pending` (never two overlapping
            // offer/answer rounds on one dialog). Hop-ACK obligation armed like
            // any non-2xx INVITE final; the peer closes it, backs off, and
            // retries. No realign sub-flow advances (the round did not complete).
            if !st.sent_reinvites.is_empty() || !st.sent_updates.is_empty() {
                uas.respond(491, "Request Pending").try_send().await?;
                arm_reject_final(st, &uas, 491);
                st.obs.record(
                    Observation::InDialogRequest {
                        leg: st.role,
                        call_id: call_id.to_string(),
                        cseq,
                        method: method.clone(),
                    },
                    now,
                );
                return Ok(());
            }
            respond_200_sdp(&mut uas, st.answer_body()).await?;
            st.answered_reinvites.insert(cseq);
            st.obs.record(
                Observation::InDialogRequest {
                    leg: st.role,
                    call_id: call_id.to_string(),
                    cseq,
                    method: method.clone(),
                },
                now,
            );
            st.obs.record(
                Observation::RequestSent {
                    key: ObligationKey::new(st.role, ObligationKind::ReInvite, cseq),
                    detail: "realign 200 awaiting ACK".to_string(),
                },
                now,
            );
            st.obs.record(
                Observation::Subflow {
                    leg: st.role,
                    name: SUBFLOW_REALIGN,
                    to: SubflowState::Answered,
                },
                now,
            );
        }
        // An UPDATE realign (RFC 3311) — answered 200 + SDP; its own 200
        // completes it (no ACK), so no obligation opens. COLLISION (C4/S6, RFC
        // 3311 §5.2): if THIS leg has its OWN offer outstanding (a re-INVITE or
        // UPDATE we sent, un-answered), the incoming UPDATE's offer glares —
        // reject it 491. Unlike the re-INVITE 491, an UPDATE's non-2xx final
        // takes NO hop-ACK (UPDATE is a non-INVITE transaction), so no
        // reject-final obligation is armed.
        "UPDATE" => {
            if !st.sent_reinvites.is_empty() || !st.sent_updates.is_empty() {
                uas.respond(491, "Request Pending").try_send().await?;
                st.obs.record(
                    Observation::InDialogRequest {
                        leg: st.role,
                        call_id: call_id.to_string(),
                        cseq,
                        method: method.clone(),
                    },
                    now,
                );
                return Ok(());
            }
            respond_200_sdp(&mut uas, st.answer_body()).await?;
            st.obs.record(
                Observation::InDialogRequest {
                    leg: st.role,
                    call_id: call_id.to_string(),
                    cseq,
                    method: method.clone(),
                },
                now,
            );
            // C5 (RFC 3311 §5.1): the EARLY UPDATE's offer/answer completed —
            // release the held INVITE 200, but only once the reliable 183 was
            // also PRACKed (MUST-014); if the UPDATE raced ahead of the PRACK
            // the PRACK arm releases it instead.
            if st.hold_for_early_update {
                st.early_updated = true;
                maybe_answer_held_invite(st).await?;
            }
        }
        // A PRACK (RFC 3262) for our reliable 183: 200 it, then — MUST-014 — this
        // is the trigger to answer 200 to the HELD INVITE txn (the reliable
        // provisional's whole point: no 200-to-INVITE before the PRACK). A
        // FORKING callee (C1/E3) answers only on the WINNER fork's PRACK — a
        // losing fork's PRACK (identified by its To-tag) is 200'd and absorbed.
        "PRACK" => {
            st.ctx.anchor(&st.agent, "prack", uas.request());
            let prack_tag = uas.request().to().tag().map(str::to_owned);
            uas.respond(200, "OK").try_send().await?;
            st.obs.record(
                Observation::InDialogRequest {
                    leg: st.role,
                    call_id: call_id.to_string(),
                    cseq,
                    method: method.clone(),
                },
                now,
            );
            // C5: this callee holds the INVITE for an early UPDATE — mark the
            // 183 PRACKed and release the held 200 only once the UPDATE is also
            // done (MUST-014 + RFC 3311 §5.1, in either arrival order).
            if st.hold_for_early_update {
                st.early_pracked = true;
                maybe_answer_held_invite(st).await?;
                return Ok(());
            }
            let releases_answer = match (&st.fork_answer, prack_tag.as_deref()) {
                // Forking: only the winner fork's PRACK releases the 200.
                (Some(f), Some(tag)) => tag == f.winner_tag,
                (Some(_), None) => false,
                // Non-forking reliable answer: any PRACK releases (as before).
                (None, _) => true,
            };
            if releases_answer {
                if let Some(inv_txn) = st.pending_prack_answer.take() {
                    let fork = st.fork_answer.take();
                    answer_initial_invite(st, inv_txn, fork).await?;
                }
            }
        }
        // Any other in-dialog method: a plain 200 (dialog-neutral).
        _ => {
            uas.respond(200, "OK").try_send().await?;
        }
    }
    Ok(())
}

/// C5: release a [`Disposition::ReliableAnswerEarlyUpdate`] callee's HELD
/// INVITE 200, but only once BOTH the reliable 183 is PRACKed (RFC 3262
/// MUST-014) AND the early UPDATE is 200'd (RFC 3311 §5.1) — regardless of the
/// order those two arrive in. A no-op otherwise.
async fn maybe_answer_held_invite(st: &mut ActorState<'_>) -> Result<(), StepError> {
    if st.hold_for_early_update && st.early_pracked && st.early_updated {
        if let Some(inv_txn) = st.pending_prack_answer.take() {
            answer_initial_invite(st, inv_txn, None).await?;
        }
    }
    Ok(())
}

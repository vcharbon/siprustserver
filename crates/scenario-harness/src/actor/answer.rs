//! The UAS answer/reject primitives shared by every path that responds to an
//! initial INVITE — the disposition entry policy ([`apply_disposition`]), the
//! single answer-200 recipe ([`answer_initial_invite`]), the reject-final
//! hop-ACK obligation ([`arm_reject_final`]), and the teardown discharge.
//! The reactive in-dialog answer table does NOT live here — see
//! [`super::react`].

use tokio::time::Instant;

use super::endpoint::Disposition;
use super::ledger::{ObligationKey, ObligationKind};
use super::runner::{ActorState, ForkAnswer, ParkedRequest, PendingRejectAck, TimedAnswer};
use super::script::scripted_wants_initial;
use super::state::Observation;
use crate::agent::top_via_branch;
use crate::{ServerTxn, StepError};

/// Open the `reject-final awaiting hop-ACK` ledger obligation for a non-2xx
/// final just sent on `uas` (§17.2.1: a real UAS keeps its server txn — and
/// this harness its recording window — until the final is ACKed, even when the
/// call has moved on). Skipped for a branch-less request, exactly like the
/// agent's own ACK-obligation table (nothing to match the hop-ACK by).
pub(super) fn arm_reject_final(st: &mut ActorState<'_>, uas: &ServerTxn, code: u16) {
    let Some(branch) = top_via_branch(uas.request()) else { return };
    let call_id = uas.request().call_id.clone();
    let key = ObligationKey::new(st.role, ObligationKind::RejectFinal, uas.request().cseq.seq);
    st.obs.record(
        Observation::RequestSent { key, detail: format!("{code} final awaiting hop-ACK") },
        Instant::now(),
    );
    st.pending_reject_ack = Some(PendingRejectAck { key, call_id: call_id.to_string(), branch });
}

/// Discharge this leg's still-open in-dialog acknowledgement obligations because
/// its DIALOG is being torn down (a BYE in either direction, §15). Any pending
/// ack — a re-INVITE we answered awaiting its ACK, a PRACK/UPDATE awaiting its
/// 200, an in-dialog request awaiting its 2xx — is MOOT once the dialog ends: the
/// far end's transaction dies with the call, so the ack can never arrive. Without
/// this the settle barrier holds the verdict its full 32 s ceiling waiting for
/// that impossible ack, surfacing under loss as a spurious `settle@…` timeout —
/// the residual re-INVITE/realign/PRACK tail where an answering/renegotiating leg
/// strands when the peer BYEs (`reinvite_gap = 0`) before a lost ack could be
/// recovered. The `RejectFinal` obligation is deliberately preserved (it outlives
/// the call on a REROUTE-abandoned leg — never a BYE — see its ledger doc). The
/// `answered_reinvites` set is drained in lockstep for hygiene.
pub(super) fn discharge_on_teardown(st: &mut ActorState<'_>, now: Instant) {
    st.answered_reinvites.clear();
    st.obs.record(Observation::DialogTornDown { leg: st.role }, now);
}

/// Answer a UAS transaction `200 OK` with the given SDP body — the single
/// respond-200 path shared by the timed answer, the reactive re-INVITE/UPDATE
/// arm, and the immediate-answer disposition. Always carries SDP on an
/// answer-to-INVITE/UPDATE (never a bodyless 200, RFC 3264 §5); callers pass the
/// media-resolved answer via [`ActorState::answer_body`].
pub(super) async fn respond_200_sdp(uas: &mut ServerTxn, sdp: &str) -> Result<(), StepError> {
    uas.respond(200, "OK").with_sdp(sdp).try_send().await
}

/// Answer a dialog-creating INVITE `200` + SDP: confirm the UAS dialog, seed
/// the dialog's received-CSeq baseline (so the first in-dialog request is not a
/// phantom hole, §12.2.1.1), open the answered-awaiting-ACK obligation (the 2xx
/// must be ACKed — the mux/SUT retransmit heals a dropped leg, and the settle
/// barrier holds the verdict until it does), and stamp the declared
/// answer-sent feed. Shared by the timed answer and the immediate disposition.
///
/// A forking callee (`fork: Some`) answers under the WINNER fork's tag (adopted
/// as the txn's dialog tag, so `uas.dialog()` keys the confirmed dialog under
/// it), then optionally emits a losing fork's LATE `200` (distinct tag on the
/// same transaction — §13.2.2.4: the caller ACKs then BYEs it). Both 200s share
/// the INVITE's CSeq, so the ONE answered-awaiting-ACK obligation covers them
/// (the wire-level per-fork ACK completeness is the RFC audit's to judge).
pub(super) async fn answer_initial_invite(
    st: &mut ActorState<'_>,
    mut uas: ServerTxn,
    fork: Option<ForkAnswer>,
) -> Result<(), StepError> {
    let sdp = st.answer_body();
    if let Some(f) = &fork {
        uas.adopt_to_tag(f.winner_tag);
    }
    respond_200_sdp(&mut uas, sdp).await?;
    note_uas_answered(st, &uas);
    if let Some(loser) = fork.and_then(|f| f.loser_late_200) {
        // The losing fork's LATE 200 — after the winner's, under the losing
        // fork's own tag (the txn's sticky tag stays the winner's).
        uas.respond(200, "OK").with_sdp(sdp).with_to_tag(loser).try_send().await?;
    }
    st.feed.on_answer_sent.stamp(st.ctx);
    Ok(())
}

/// The bookkeeping a 2xx to a dialog-creating INVITE requires, response
/// already sent: confirm the UAS dialog, seed the received-CSeq baseline
/// (§12.2.1.1), and open the answered-awaiting-ACK obligation. Shared by the
/// policy answer and the scripted `Respond`/`RespondTemplate` finals.
pub(super) fn note_uas_answered(st: &mut ActorState<'_>, uas: &ServerTxn) {
    let call_id = uas.request().call_id.clone();
    let cseq = uas.request().cseq.seq;
    let now = Instant::now();
    let mut dialog = uas.dialog();
    // Dialog-formation point: attach this leg's shared CSeq counter (ADR-0024 §6).
    dialog.set_shared_cseq_dev(st.cseq_dev.clone());
    st.dialogs.confirmed = Some(dialog);
    st.obs.record(Observation::SeedDialog { leg: st.role, call_id: call_id.to_string(), cseq }, now);
    st.obs.record(
        Observation::RequestSent {
            key: ObligationKey::new(st.role, ObligationKind::ReInvite, cseq),
            detail: "answered 2xx awaiting ACK".to_string(),
        },
        now,
    );
}

/// Fire a due timed answer: `200` + the answer SDP on the retained INVITE txn,
/// then confirm the UAS dialog. (Called only when `pending_answer` is `Some`.)
pub(super) async fn fire_timed_answer(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let Some(ta) = st.pending_answer.take() else {
        return Ok(());
    };
    answer_initial_invite(st, ta.uas, ta.fork).await
}

/// Apply the endpoint's initial-INVITE disposition — the entry policy of the
/// endpoint state machine.
pub(super) async fn apply_disposition(
    st: &mut ActorState<'_>,
    mut uas: ServerTxn,
) -> Result<(), StepError> {
    let now = Instant::now();
    // A fresh initial INVITE opens a fresh early-dialog space — a stale count
    // from a previous initial (a reroute-retry to this actor) must not leak
    // into the policy snapshot. A retransmit re-inserts the same tags.
    st.early_provisionals.clear();
    st.ctx.anchor(&st.agent, "initialInvite", uas.request());
    // The rerouted winning leg stamps `rerouted` on receiving its INVITE
    // (default NONE for every other body — see `CtxFeed::on_invite_rx`).
    st.feed.on_invite_rx.stamp(st.ctx);
    match st.disposition {
        // A caller should never receive an initial INVITE; answer it defensively
        // so a wiring bug doesn't strand the peer. `Answer` is the immediate
        // no-provisional answer.
        Disposition::Caller | Disposition::Answer => {
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            answer_initial_invite(st, uas, None).await?;
        }
        Disposition::RingThenAnswer { ring } => {
            uas.respond(180, "Ringing").try_send().await?;
            st.early_provisionals.insert(String::new());
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            st.pending_answer = Some(TimedAnswer { at: Instant::now() + ring, uas, fork: None });
        }
        // Ring then SILENCE: the 180 goes out, the INVITE server txn is
        // held with NO answer ever scheduled — the SUT's own no-answer timer
        // must end this leg. Its CANCEL lands in `react_request`'s CANCEL arm,
        // which takes the held txn and 487s it (arming the reject-final
        // obligation).
        Disposition::RingThenSilent => {
            uas.respond(180, "Ringing").try_send().await?;
            st.early_provisionals.insert(String::new());
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            st.held_silent = Some(uas);
        }
        // C1/E3 forking UAS: one 18x per DISTINCT explicit To-tag on the ONE
        // retained INVITE server txn (as if a downstream proxy forked), then the
        // 200 under the winner's tag — timed (plain 180s) or on the winner's
        // PRACK (reliable 183s, MUST-014).
        Disposition::ForkingRing { tags, winner, ring, reliable, loser_late_200 } => {
            let wired_ok = tags.contains(&winner)
                && loser_late_200.is_none_or(|l| l != winner && tags.contains(&l));
            if !wired_ok {
                return Err(StepError::UnexpectedKind {
                    who: st.role.to_string(),
                    detail: "ForkingRing winner/loser must be distinct declared fork tags"
                        .to_string(),
                });
            }
            let sdp = st.answer_body();
            for tag in tags {
                if reliable {
                    // RFC 3262 §3: each fork's reliable 183 carries its own RSeq
                    // space (RSeq:1 per early dialog) + the answer SDP.
                    uas.respond(183, "Session Progress")
                        .with_to_tag(tag)
                        .reliable(1)
                        .with_sdp(sdp)
                        .try_send()
                        .await?;
                } else {
                    uas.respond(180, "Ringing").with_to_tag(tag).try_send().await?;
                }
                st.early_provisionals.insert((*tag).to_string());
            }
            if let Some(loser) = loser_late_200 {
                st.fork_loser_tags.insert(loser.to_string());
            }
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            let fork = ForkAnswer { winner_tag: winner, loser_late_200 };
            if reliable {
                // The 200 waits for the WINNER fork's PRACK (see the PRACK arm).
                st.fork_answer = Some(fork);
                st.pending_prack_answer = Some(uas);
            } else {
                st.pending_answer =
                    Some(TimedAnswer { at: Instant::now() + ring, uas, fork: Some(fork) });
            }
        }
        Disposition::Reject(code) => {
            uas.respond(code, reject_reason(code)).try_send().await?;
            arm_reject_final(st, &uas, code);
            st.obs.record(Observation::LegTerminated { leg: st.role }, now);
            st.scope.mark_terminated();
        }
        // RFC 3262: answer RELIABLY with a 183 (Require:100rel + RSeq:1 + the
        // answer SDP) and HOLD the INVITE txn — the 200 to the INVITE waits for
        // the PRACK (MUST-014, fired from the PRACK arm of `react_request`).
        // Both reliable-answer dispositions emit the same reliable 183 and HOLD
        // the INVITE. They differ only in WHEN the held 200 is released: the
        // plain one on the PRACK (MUST-014); the early-UPDATE one after the
        // early UPDATE is answered (C5, RFC 3311 §5.1 — see the PRACK/UPDATE arms).
        Disposition::ReliableAnswer | Disposition::ReliableAnswerEarlyUpdate => {
            let sdp = st.answer_body();
            uas.respond(183, "Session Progress").reliable(1).with_sdp(sdp).try_send().await?;
            st.early_provisionals.insert(String::new());
            st.obs.record(Observation::LegEarly { leg: st.role }, now);
            st.pending_prack_answer = Some(uas);
        }
        // Scripted park-or-react for the initial INVITE: park when a remaining
        // goal will consume/answer it, else auto-answer 200 (the RFC-compliant
        // react default) and record the stray. The ADR-0024 §5 automatic answers `100
        // Trying` in both cases — it never consumes the transaction.
        Disposition::Scripted => {
            if st.automatics.answer_100_trying {
                uas.respond(100, "Trying").try_send().await?;
            }
            if scripted_wants_initial(st) {
                st.parked.push(ParkedRequest { txn: uas, initial: true });
            } else {
                st.obs.record(
                    Observation::ServicedStray {
                        leg: st.role,
                        method: "INVITE".to_string(),
                        action: "auto-answered 200",
                    },
                    now,
                );
                st.obs.record(Observation::LegEarly { leg: st.role }, now);
                answer_initial_invite(st, uas, None).await?;
            }
        }
    }
    Ok(())
}

/// The stock reason phrase for a policy provisional.
pub(super) fn provisional_reason(status: u16) -> &'static str {
    match status {
        180 => "Ringing",
        183 => "Session Progress",
        _ => "Progress",
    }
}

/// The stock reason phrase for a rejection disposition's status code.
pub(super) fn reject_reason(code: u16) -> &'static str {
    match code {
        486 => "Busy Here",
        603 => "Decline",
        487 => "Request Terminated",
        480 => "Temporarily Unavailable",
        _ => "Error",
    }
}

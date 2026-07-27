//! The reactive response fold — everything an inbound response drives on a
//! caller: the establishing-INVITE flow (provisional/PRACK, answer, §22.2 auth
//! retry, failure), fork late-2xx handling, re-INVITE/UPDATE 2xx + 491 glare
//! completion, and obligation-closing for sent in-dialog requests. Inbound
//! REQUESTS do NOT fold here — see [`super::react`].

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use sip_message::generators::InDialogMethod;
use sip_message::SipResponse;

use super::endpoint::{SUBFLOW_EARLY, SUBFLOW_REFER, SUBFLOW_RENEG};
use super::goals::GoalStep;
use super::ledger::{ObligationKey, ObligationKind};
use super::runner::ActorState;
use super::state::{Observation, ResponseFact, SubflowState};
use crate::agent::InviteResponseFate;
use crate::{ClientInvite, StepError};

/// A provisional on the caller's establishing INVITE: anchor/feed the first
/// over-100, and PRACK a reliable one exactly once per `(fork tag, RSeq)` — a
/// retransmitted 183 is not double-PRACKed, while a FORKED 183 (distinct
/// To-tag, RFC 3261 §12.1.2) gets its OWN PRACK on its own early dialog. The
/// PRACK opens an "awaiting 200" ledger obligation the settle barrier holds on.
async fn absorb_establishing_provisional(
    st: &mut ActorState<'_>,
    inv: &mut ClientInvite,
    resp: &SipResponse,
    status: u16,
    now: Instant,
) -> Result<(), StepError> {
    // A 100 Trying is transaction plumbing, not an early dialog.
    if status <= 100 {
        return Ok(());
    }
    st.obs.record(Observation::LegEarly { leg: st.role }, now);
    if !st.saw_provisional {
        st.saw_provisional = true;
        st.ctx.anchor(&st.agent, "firstProvisional", resp);
        // The abandon body's `time_to_180` (default NONE on every other body).
        st.feed.on_provisional.stamp(st.ctx);
        if st.feed.ringing_gate {
            st.ctx.mark_ringing(true);
        }
    }
    if let Some(rseq) = reliable_rseq(resp) {
        let fork = resp.to().tag().map(str::to_owned).unwrap_or_default();
        if st.pracked_rseqs.insert((fork.to_string(), rseq)) {
            let (_txn, req) = inv.try_prack_with_request(resp).await?;
            st.obs.record(
                Observation::RequestSent {
                    key: ObligationKey::new(st.role, ObligationKind::Prack, req.cseq().seq()),
                    detail: "prack awaiting 200".to_string(),
                },
                now,
            );
            // C5: the reliable early dialog is now established AND PRACKed —
            // the observed fact an early UPDATE (RFC 3311 §5.1) gates on (a
            // real post-183 signal, unlike `LegPhase::Early` which holds
            // pre-183).
            st.obs.record(
                Observation::Subflow {
                    leg: st.role,
                    name: SUBFLOW_EARLY,
                    to: SubflowState::Answered,
                },
                now,
            );
        }
    }
    Ok(())
}

/// A non-2xx final on the caller's establishing INVITE. Returns `true` when a
/// §22.2 authenticated resend consumed the challenge (caller re-parks the
/// INVITE); otherwise records the terminal and — unless the next pending goal
/// is a RECEPTION goal, which then owns the verdict — surfaces the incidental
/// establishment failure as the linear `WrongStatus{expected: <180|183>}`,
/// never a 32 s barrier timeout.
async fn absorb_establishing_failure(
    st: &mut ActorState<'_>,
    inv: &mut ClientInvite,
    resp: &SipResponse,
    status: u16,
    now: Instant,
) -> Result<bool, StepError> {
    // RFC 3261 §22.2 authenticated retry: `absorb_response` already ACKed the
    // challenge (§17.1.1.3), so this goes straight to asking the responder for
    // a credential and resending ONCE (bumped CSeq, fresh branch). A second
    // challenge has `auth_retries_left == 0` and classifies as a plain
    // `status_401/407` deviation — never an unbounded loop.
    if matches!(status, 401 | 407) && st.auth_retries_left > 0 {
        if let Some(responder) = st.challenge_responder.clone() {
            if inv.ack_and_resend_with_auth(resp, responder.as_ref()).await? {
                st.auth_retries_left -= 1;
                // Re-point the scope's early CANCEL handle at the retried
                // transaction (its branch/CSeq changed).
                st.scope.set_early(inv.cancel_handle());
                return Ok(true);
            }
            // Responder DECLINED — surface the challenge as a plain deviation
            // (status_401/407), exactly as with no responder.
        }
    }
    st.obs.record(
        Observation::LegFinal { leg: st.role, status, reason: resp.reason().to_string() },
        now,
    );
    st.obs.record(Observation::LegTerminated { leg: st.role }, now);
    st.scope.mark_terminated();
    if st.goals.has_pending() && !st.goals.next_step().is_some_and(GoalStep::is_reception) {
        return Err(StepError::WrongStatus {
            who: st.role.to_string(),
            expected: st.expected_provisional,
            got: status,
            reason: resp.reason().to_string(),
        });
    }
    Ok(false)
}

/// Fold one inbound response into this leg's ordered response-fact log. The
/// typed message is retained only while a matcher-carrying reception goal is
/// still pending on this actor (the content matcher compares it later).
pub(super) fn record_response_fact(st: &mut ActorState<'_>, resp: &SipResponse, now: Instant) {
    let retain = st
        .goals
        .remaining_steps()
        .any(|s| matches!(s, GoalStep::ExpectResponse { matcher: Some(_), .. }));
    let body_is_sdp = !resp.body().is_empty()
        && resp
            .header::<sip_message::header::MediaType>()
            .and_then(Result::ok)
            .is_some_and(|media| media.token().to_ascii_lowercase().contains("sdp"));
    st.obs.record(
        Observation::LegResponse {
            leg: st.role,
            fact: ResponseFact {
                status: resp.status(),
                reason: resp.reason().to_string(),
                body_len: resp.body().len(),
                body_is_sdp,
                early_tag: resp.to().tag().map(str::to_string),
                typed: retain.then(|| Box::new(resp.clone())),
            },
        },
        now,
    );
}

pub(super) async fn react_response(st: &mut ActorState<'_>, resp: SipResponse) -> Result<(), StepError> {
    let now = Instant::now();
    record_response_fact(st, &resp, now);
    // A response to our still-pending caller INVITE drives the establish flow —
    // but ONLY a response whose CSeq method is INVITE. A PRACK's 200 (or any
    // other in-dialog final) sharing the early dialog must NOT be fed to the
    // INVITE transaction (`absorb_response` would misread a PRACK 200 as the
    // INVITE being answered); it falls through to the obligation-closing path.
    if resp.cseq().method() == "INVITE" {
        if let Some(mut inv) = st.dialogs.pending_invite.take() {
        match inv.absorb_response(&resp).await? {
            InviteResponseFate::Provisional { status } => {
                absorb_establishing_provisional(st, &mut inv, &resp, status, now).await?;
                st.dialogs.pending_invite = Some(inv);
            }
            InviteResponseFate::Answered => {
                st.ctx.anchor(&st.agent, "answer", &resp);
                if st.feed.ringing_gate && !st.saw_provisional {
                    // Answered without ever ringing: a lost non-PRACK 18x is
                    // best-effort — counted into the cross-call gate, never a
                    // per-call failure (contract table §3).
                    st.ctx.mark_ringing(false);
                }
                st.feed.on_answer_rx.stamp(st.ctx);
                // ACK the 2xx then register the confirmed dialog with NO await in
                // between, so a mid-window cancellation can never leave a
                // confirmed-but-unregistered dialog (the drop-safety rule).
                let mut dialog = inv.ack().await;
                // Dialog-formation point: attach this leg's shared CSeq counter
                // BEFORE the scope-refresh clone, so both share ONE step counter
                // (ADR-0024 §6 — the teardown BYE never re-consumes an op).
                dialog.set_shared_cseq_dev(st.cseq_dev.clone());
                st.dialogs.confirmed = Some(dialog.clone());
                st.scope.set_confirmed(dialog);
                st.obs.record(Observation::LegConfirmed { leg: st.role }, now);
                // RETAIN the establishing INVITE (C1/E3): a LOSING fork's late
                // 2xx (§13.2.2.4) is ACK+BYE'd on a fork dialog derived from it.
                st.dialogs.won_invite = Some(inv);
            }
            InviteResponseFate::Failed { status } => {
                if absorb_establishing_failure(st, &mut inv, &resp, status, now).await? {
                    // §22.2 authenticated resend — the retried INVITE is a
                    // fresh pending transaction, parked back.
                    st.dialogs.pending_invite = Some(inv);
                }
            }
        }
        return Ok(());
        }
        // A NON-2xx final to a re-INVITE WE originated (C4/S5 glare): a `491
        // Request Pending` (§14.1) the peer sent because it had its OWN re-INVITE
        // outstanding when ours arrived. Hop-ACK it (§17.1.1.3 — `recv_any` does
        // not), CLOSE its ReInvite obligation (so a 491'd re-INVITE leaves no
        // open obligation), and schedule a RETRY after the §14.1 owner/non-owner
        // dwell (the dialog owner — the caller — backs off longer, so the two
        // retries no longer collide).
        if resp.cseq().method() == "INVITE"
            && (300..700).contains(&resp.status())
            && st.sent_reinvites.contains(&resp.cseq().seq())
        {
            if let Some(txn) = st.sent_reinvite_txns.remove(&resp.cseq().seq()) {
                txn.ack_non_2xx(&resp).await?;
            }
            st.sent_reinvites.remove(&resp.cseq().seq());
            st.obs.record(
                Observation::ResponseObserved {
                    key: ObligationKey::new(st.role, ObligationKind::ReInvite, resp.cseq().seq()),
                },
                now,
            );
            if resp.status() == 491 {
                // §14.1: the owner of the Call-ID (the dialog's original UAC —
                // the ORIGINATING actor, keyed on its first goal) waits a random
                // T in [2.1, 4] s; a non-owner in [0, 2] s. Fixed in-range
                // values keep the paused-clock test deterministic while
                // preserving the owner>non-owner ordering that breaks the glare.
                let dwell = if st.originates {
                    Duration::from_millis(2500)
                } else {
                    Duration::from_millis(1000)
                };
                st.reinvite_retry = Some(Instant::now() + dwell);
            }
            return Ok(());
        }
        // A LATE 2xx from a LOSING fork (C1/E3, RFC 3261 §13.2.2.4): it echoes
        // the ESTABLISHING INVITE's CSeq but carries a DIFFERENT To-tag than the
        // confirmed (winner) dialog — a separate dialog this caller never chose.
        // ACK it on ITS OWN fork dialog (the ACK carries the fork's tag) then
        // terminate that fork with an immediate BYE. The BYE opens a `ForkBye`
        // obligation (closed by its tag-mismatched 200 below — NEVER terminating
        // this leg; the winning dialog lives on). Checked BEFORE the re-INVITE
        // 2xx path: a re-INVITE's 2xx always carries the confirmed tag.
        if (200..300).contains(&resp.status()) {
            let is_losing_fork = st
                .dialogs
                .won_invite
                .as_ref()
                .is_some_and(|inv| inv.invite_cseq() == resp.cseq().seq())
                && st
                    .dialogs
                    .confirmed
                    .as_ref()
                    .zip(resp.to().tag().as_ref())
                    .is_some_and(|(d, t)| d.remote_tag() != *t);
            if is_losing_fork {
                if let Some(inv) = st.dialogs.won_invite.as_ref() {
                    let mut fork = inv.fork_dialog(&resp);
                    // Our INVITE carried the offer, so the fork's 200 carried
                    // its answer — the ACK is bodyless (§13.2.2.4).
                    fork.ack_for(resp.cseq().seq(), None).await;
                    let _bye =
                        fork.send_request(InDialogMethod::Bye).try_send().await?;
                    st.obs.record(
                        Observation::RequestSent {
                            key: ObligationKey::new(
                                st.role,
                                ObligationKind::ForkBye,
                                fork.local_cseq(),
                            ),
                            detail: "losing-fork hangup awaiting 200".to_string(),
                        },
                        now,
                    );
                }
                return Ok(());
            }
        }
        // A 2xx to an in-dialog INVITE with NO pending initial INVITE: this
        // caller's own delayed-offer re-INVITE (the `reinvite` body). ACK it WITH
        // the answer SDP (RFC 3264 §4 delayed offer) — IDEMPOTENTLY, re-derived
        // from the confirmed dialog + `resp.cseq`, NEVER gated on a one-shot a
        // lost-datagram interleaving could strand (that stranding is the bug this
        // fixes — mirrors the mux's `(Call-ID, CSeq)` re-ACK).
        // Every such 2xx the reactor is handed is ACKed; closing the `ReInvite`
        // obligation, advancing the `reneg` teardown barrier, and stamping the
        // feed happen ONCE, keyed on the CSeq of a re-INVITE THIS leg originated.
        if (200..300).contains(&resp.status()) && st.dialogs.confirmed.is_some() {
            let default = st.answer_body();
            let sdp = resolve_ack_body(
                &mut st.reinvite_ack_bodies,
                st.goals.next_step(),
                default,
                resp.cseq().seq(),
            );
            if let Some(dialog) = st.dialogs.confirmed.as_mut() {
                dialog.ack_for(resp.cseq().seq(), Some(&sdp)).await;
            }
            if st.sent_reinvites.remove(&resp.cseq().seq()) {
                st.sent_reinvite_txns.remove(&resp.cseq().seq());
                let key = ObligationKey::new(st.role, ObligationKind::ReInvite, resp.cseq().seq());
                st.obs.record(Observation::ResponseObserved { key }, now);
                st.obs.record(
                    Observation::Subflow { leg: st.role, name: SUBFLOW_RENEG, to: SubflowState::Confirmed },
                    now,
                );
                // Count this completed cycle so an N-cycle re-INVITE script's
                // per-cycle barrier (reneg_count >= i) releases the next one —
                // serializing the chain (C6). Keyed on CSeq: a re-emitted 2xx
                // (a retransmit under loss) cannot double-count, and the
                // sent_reinvites guard already fires this block once per CSeq.
                st.obs.record(Observation::RenegCompleted { leg: st.role, cseq: resp.cseq().seq() }, now);
                st.feed.on_reinvite_ok.stamp(st.ctx);
            }
            return Ok(());
        }
    }

    // A 491 to an UPDATE WE originated (C4/S6 collision): close its Update
    // obligation and RETRY after the back-off. UPDATE has NO ACK, so the 491
    // alone completes the transaction — nothing to hop-ACK. (The owner/non-owner
    // dwell mirrors §14.1 for a deterministic, glare-breaking retry order.)
    if resp.cseq().method() == "UPDATE"
        && resp.status() == 491
        && st.sent_updates.remove(&resp.cseq().seq())
    {
        st.obs.record(
            Observation::ResponseObserved {
                key: ObligationKey::new(st.role, ObligationKind::Update, resp.cseq().seq()),
            },
            now,
        );
        let dwell = if st.originates {
            Duration::from_millis(2500)
        } else {
            Duration::from_millis(1000)
        };
        st.update_retry = Some(Instant::now() + dwell);
        return Ok(());
    }

    // Otherwise it is a final to one of our sent in-dialog requests (our BYE's
    // 200, our REFER's 202, our NOTIFY's 200, our PRACK's 200, …) — close the
    // obligation it opened and stamp the declared feed for the flow-advancing ones.
    if let Some(kind) = ObligationKind::from_cseq_method(resp.cseq().method().as_str()) {
        // The 200 to a LOSING-FORK BYE (C1/E3): same CSeq method (and possibly
        // the same CSeq number — fork spaces are independent, §12.2.1.1) as the
        // main BYE, but its To-tag echoes the LOSING fork's, not the confirmed
        // (winner) dialog's. It closes the `ForkBye` obligation WITHOUT
        // terminating this leg — the winning dialog lives on.
        let fork_teardown = kind == ObligationKind::Bye
            && st
                .dialogs
                .confirmed
                .as_ref()
                .zip(resp.to().tag().as_ref())
                .is_some_and(|(d, t)| d.remote_tag() != *t);
        let kind = if fork_teardown { ObligationKind::ForkBye } else { kind };
        let key = ObligationKey::new(st.role, kind, resp.cseq().seq());
        st.obs.record(Observation::ResponseObserved { key }, now);
        if (200..300).contains(&resp.status()) {
            match kind {
                ObligationKind::Bye => {
                    st.obs.record(Observation::LegTerminated { leg: st.role }, now);
                    st.scope.mark_terminated();
                    st.feed.on_bye_ok.stamp(st.ctx);
                }
                ObligationKind::Refer => {
                    st.obs.record(
                        Observation::Subflow {
                            leg: st.role,
                            name: SUBFLOW_REFER,
                            to: SubflowState::Answered,
                        },
                        now,
                    );
                    st.feed.on_refer_accepted.stamp(st.ctx);
                }
                // The 200 to our PRACK — the 100rel flows' `pracked` /
                // `time_to_prack_200` (the reliable provisional is acknowledged).
                ObligationKind::Prack => st.feed.on_prack_ok.stamp(st.ctx),
                // The 200 to our in-dialog UPDATE — the `prack_update` flow's
                // `updated` / `time_to_update_200` (no ACK; the 200 completes it).
                // Advance the caller's `reneg` sub-flow so the teardown barrier
                // holds before the BYE.
                ObligationKind::Update => {
                    st.sent_updates.remove(&resp.cseq().seq());
                    st.obs.record(
                        Observation::Subflow {
                            leg: st.role,
                            name: SUBFLOW_RENEG,
                            to: SubflowState::Confirmed,
                        },
                        now,
                    );
                    // Count the completed renegotiation uniformly with a
                    // re-INVITE (C6/S6), so a glare barrier can gate on
                    // `reneg_count` regardless of the offer's method.
                    st.obs.record(
                        Observation::RenegCompleted { leg: st.role, cseq: resp.cseq().seq() },
                        now,
                    );
                    st.feed.on_update_ok.stamp(st.ctx);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The body of the ACK to an in-dialog INVITE 2xx: the pending
/// `ExpectResponse`'s `ack_body` override, else `default` (the engine-built
/// answer SDP) — resolved ONCE per CSeq and cached, so the ACK to a
/// re-surfaced 2xx is byte-identical (RFC 3261 §13.2.2.4) even after the goal
/// cursor advanced past the override-carrying goal.
pub(super) fn resolve_ack_body(
    cache: &mut HashMap<u32, String>,
    next_step: Option<&GoalStep>,
    default: &str,
    cseq: u32,
) -> String {
    if let Some(cached) = cache.get(&cseq) {
        return cached.clone();
    }
    let resolved = match next_step {
        Some(GoalStep::ExpectResponse { ack_body: Some(b), .. }) => {
            String::from_utf8_lossy(b).into_owned()
        }
        _ => default.to_string(),
    };
    cache.insert(cseq, resolved.clone());
    resolved
}

/// The `RSeq` of a reliable provisional (RFC 3262) — `Some(rseq)` iff `resp`
/// carries a parseable `RSeq` header (marking it PRACK-required), else `None`.
fn reliable_rseq(resp: &SipResponse) -> Option<u32> {
    Some(resp.header::<sip_message::header::RSeq>()?.ok()?.value())
}

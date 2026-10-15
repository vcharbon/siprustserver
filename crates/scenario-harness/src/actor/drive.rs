//! The goal-step dispatcher — turns one due [`GoalStep`] into wire activity
//! and ledger bookkeeping. Origination recipes live in [`super::originate`];
//! scripted reception/respond realizations live in [`super::script`].

use tokio::time::Instant;

use sip_message::generators::InDialogMethod;

use super::answer::discharge_on_teardown;
use super::goals::{FinalAssert, GoalStep};
use super::held::{answer_held, callee_early_request, ring};
use super::ledger::{ObligationKey, ObligationKind};
use super::originate::{
    originate_early, originate_in_dialog, originate_initial_invite, originate_reinvite,
    originate_update, ping_options_once, send_request_template,
};
use super::runner::ActorState;
use super::script::{consume_final_fact, drive_respond, expect_request, expect_response};
use super::state::Observation;
use crate::StepError;

/// Drive one scripted goal step.
pub(super) async fn drive_goal(st: &mut ActorState<'_>, step: GoalStep) -> Result<(), StepError> {
    match step {
        GoalStep::Invite { callee, plan } => {
            originate_initial_invite(st, callee, plan, None).await?;
        }
        // The template twin of `Invite`: frozen headers/body ride verbatim,
        // routing and bookkeeping identical.
        GoalStep::InviteTemplate { callee, plan, template, opts } => {
            if template.method().map(|m| m.as_str()) != Some("INVITE") {
                return Err(StepError::UnexpectedKind {
                    who: st.role.to_string(),
                    detail: "InviteTemplate requires an INVITE request template".to_string(),
                });
            }
            originate_initial_invite(st, callee, plan, Some((template, opts))).await?;
        }
        // An in-dialog (or early-dialog) request from a template; method read
        // from the template. Opens the method's ledger obligation.
        GoalStep::RequestTemplate { template, opts, early } => {
            send_request_template(st, &template, opts, early).await?;
        }
        // Answer the bound server transaction from the template (status/reason
        // read from it) — provisional-non-consuming, final-consuming.
        GoalStep::RespondTemplate { template, opts, early } => {
            let Some((status, _)) = template.status() else {
                return Err(StepError::UnexpectedKind {
                    who: st.role.to_string(),
                    detail: "RespondTemplate requires a response template".to_string(),
                });
            };
            drive_respond(st, status, Some((&template, opts)), early, "RespondTemplate").await?;
        }
        // Answer the bound server transaction by POLICY — the completion verb.
        GoalStep::Respond { status } => {
            drive_respond(st, status, None, None, "Respond").await?;
        }
        GoalStep::ExpectResponse { status, cseq_method, body, early, ack_body: _, matcher } => {
            expect_response(st, status, cseq_method.as_deref(), body, early, matcher.as_ref())?;
        }
        GoalStep::ExpectRequest { kind, body, matcher, rank } => {
            expect_request(st, &kind, body, rank, matcher.as_ref())?;
        }
        GoalStep::ObserveFinal { key, expected, cseq_method } => {
            let fact = consume_final_fact(st, cseq_method.as_deref())?;
            st.obs.record(
                Observation::ReplayFinal { key, expected, observed: fact.status },
                Instant::now(),
            );
        }
        GoalStep::ExpectFinal { assert, cseq_method } => {
            let fact = consume_final_fact(st, cseq_method.as_deref())?;
            let (ok, want) = match assert {
                FinalAssert::Exact(s) => (fact.status == s, s),
                FinalAssert::Class(c) => (fact.status / 100 == c, c * 100),
                FinalAssert::NonError => (fact.status < 400, 200),
            };
            if !ok {
                return Err(StepError::WrongStatus {
                    who: st.role.to_string(),
                    expected: want,
                    got: fact.status,
                    reason: fact.reason,
                });
            }
        }
        GoalStep::Refer { refer_to, authorization } => {
            let now = Instant::now();
            let (key, dialog_clone, request) = {
                let dialog =
                    st.dialogs.confirmed.as_mut().ok_or_else(|| StepError::UnexpectedKind {
                        who: st.role.to_string(),
                        detail: "Refer goal with no confirmed dialog".to_string(),
                    })?;
                let mut refer =
                    dialog.send_request(InDialogMethod::Refer).with_header("Refer-To", &refer_to);
                if let Some(api) = &authorization {
                    refer = refer.with_header("X-Api-Call", api);
                }
                // The 202 arrives through the reactor (recv_any) — the returned
                // transaction handle is not awaited on.
                let (_txn, request) = refer.try_send_with_request().await?;
                let key = ObligationKey::new(st.role, ObligationKind::Refer, request.cseq().seq());
                (key, dialog.clone(), request)
            };
            // The REFER's only receiver is the SUT itself (it builds the C leg),
            // so it is anchored as a SENT message on this leg's lane.
            st.ctx.anchor_sent(&st.agent, "refer", &request);
            st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
            st.obs.record(
                Observation::RequestSent { key, detail: "refer awaiting 202".to_string() },
                now,
            );
        }
        // A delayed-offer (bodyless) re-INVITE: send it, open the ReInvite
        // obligation keyed on its CSeq; the reactor ACKs the 2xx with the answer
        // SDP and stamps `on_reinvite_ok` (see `react_response`).
        GoalStep::Reinvite => {
            originate_reinvite(st).await?;
        }
        // An in-dialog UPDATE (RFC 3311) carrying this leg's offer: send it, open
        // the Update obligation; its 200 closes it (no ACK) and stamps
        // `on_update_ok` (see `react_response`).
        GoalStep::Update => {
            originate_update(st).await?;
        }
        // C5 (RFC 3311 §5.1): an EARLY UPDATE on the still-pending INVITE's
        // EARLY dialog (its reliable provisional already PRACKed); its 200
        // releases the callee's held INVITE 200.
        GoalStep::UpdateEarly => {
            originate_early(st, InDialogMethod::Update).await?;
        }
        // A request on an early dialog: the caller's pending INVITE's, or a
        // held callee's own.
        GoalStep::EarlyRequest { method, fork } => {
            if st.dialogs.pending_invite.is_some() {
                originate_early(st, method).await?;
            } else {
                callee_early_request(st, method, fork).await?;
            }
        }
        GoalStep::Ring { forks, reliable } => ring(st, forks, reliable).await?,
        GoalStep::Answer { winner } => answer_held(st, winner).await?,
        // One in-dialog OPTIONS keepalive ping, its 200 read inline (the reactor
        // has nothing else to do for this leg during the ping).
        GoalStep::Options => {
            ping_options_once(st).await?;
        }
        // The OPTIONS-keepalive hold loop: ping every `cadence` until `hold`
        // elapses. Each 200 is read inline; the first stamps `keepalive_ack`.
        GoalStep::EveryOptions { cadence, hold } => {
            let start = Instant::now();
            while start.elapsed() < hold {
                tokio::time::sleep(cadence).await;
                ping_options_once(st).await?;
            }
        }
        // CANCEL the still-pending initial INVITE (RFC 3261 §9.1). KEEP the
        // pending INVITE so its `487` still routes to it (→ `Failed{487}` →
        // LegTerminated); the peer's CANCEL→200+487 is handled reactively. The
        // CANCEL's own transaction is an obligation its final closes, so the
        // call settles only once the server answered it.
        GoalStep::Cancel { stated } => {
            if let Some(inv) = st.dialogs.pending_invite.as_ref() {
                let stated: Vec<(&str, &str)> =
                    stated.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
                let cseq = inv.invite_cseq();
                let _cxl = inv.try_cancel_stating(&stated).await?;
                let now = Instant::now();
                st.obs.record(Observation::CancelSent { leg: st.role }, now);
                st.obs.record(
                    Observation::RequestSent {
                        key: ObligationKey::new(st.role, ObligationKind::Cancel, cseq),
                        detail: "CANCEL awaiting its final".to_string(),
                    },
                    now,
                );
            }
        }
        // A plain in-dialog request (INFO/MESSAGE) carrying an optional typed
        // body + extra headers: send it on the confirmed dialog and open the
        // InDialog obligation keyed on its CSeq. Its 2xx closes it (no ACK, no
        // sub-flow — the `_` arm of `react_response`'s obligation match); a
        // dropped request or its 2xx holds the settle barrier until re-emitted,
        // exactly like a lost NOTIFY.
        GoalStep::InDialog { method, content_type, body, headers } => {
            originate_in_dialog(st, method, content_type, body, headers).await?;
        }
        GoalStep::Bye => {
            let now = Instant::now();
            // This leg is hanging up: any HELD ACK goes out first (§15 — the
            // renegotiation completes before the dialog ends), then discharge
            // any in-dialog ack it still awaits (a re-INVITE/realign it
            // answered, a PRACK/UPDATE 200) BEFORE opening the BYE's own
            // obligation — the terminating dialog subsumes them, and the fresh
            // BYE obligation below is still held to the 200.
            super::held_acks::flush_held_acks(st).await?;
            discharge_on_teardown(st, now);
            let (key, dialog_clone) = {
                let dialog =
                    st.dialogs.confirmed.as_mut().ok_or_else(|| StepError::UnexpectedKind {
                        who: st.role.to_string(),
                        detail: "Bye goal with no confirmed dialog".to_string(),
                    })?;
                // Send the BYE; the reactor observes its 200 and closes the
                // obligation (we do not block on the final here).
                let _bye = dialog.try_bye().await?;
                let cseq = dialog.local_cseq();
                (ObligationKey::new(st.role, ObligationKind::Bye, cseq), dialog.clone())
            };
            st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
            st.obs.record(Observation::RequestSent { key, detail: "hangup".to_string() }, now);
        }
        // Branch-conditional teardown (C2/E5): BYE the confirmed dialog if one
        // exists (the 200-wins branch), else a NO-OP (the CANCEL-wins branch —
        // the leg was 487'd, there is nothing to tear down). Same obligation
        // bookkeeping as `Bye` when a dialog exists.
        GoalStep::ByeIfConfirmed => {
            if st.dialogs.confirmed.is_none() {
                return Ok(());
            }
            let now = Instant::now();
            super::held_acks::flush_held_acks(st).await?; // see GoalStep::Bye
            discharge_on_teardown(st, now);
            let (key, dialog_clone) = {
                let dialog = st.dialogs.confirmed.as_mut().expect("checked Some above");
                let _bye = dialog.try_bye().await?;
                let cseq = dialog.local_cseq();
                (ObligationKey::new(st.role, ObligationKind::Bye, cseq), dialog.clone())
            };
            st.scope.set_confirmed(dialog_clone);
            st.obs.record(Observation::RequestSent { key, detail: "hangup".to_string() }, now);
        }
        GoalStep::ByeWith { headers } => {
            let now = Instant::now();
            super::held_acks::flush_held_acks(st).await?; // see GoalStep::Bye
            discharge_on_teardown(st, now); // see GoalStep::Bye — subsume pending in-dialog acks
            let (key, dialog_clone) = {
                let dialog =
                    st.dialogs.confirmed.as_mut().ok_or_else(|| StepError::UnexpectedKind {
                        who: st.role.to_string(),
                        detail: "ByeWith goal with no confirmed dialog".to_string(),
                    })?;
                // Send the BYE carrying the extra headers (the deliberate
                // deviation); the reactor observes its 200 and closes the
                // obligation (we do not block on the final here).
                let mut req = dialog.send_request(InDialogMethod::Bye);
                for (name, value) in &headers {
                    req = req.with_header(name, value);
                }
                let _bye = req.try_send().await?;
                let cseq = dialog.local_cseq();
                (ObligationKey::new(st.role, ObligationKind::Bye, cseq), dialog.clone())
            };
            st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
            st.obs.record(Observation::RequestSent { key, detail: "hangup".to_string() }, now);
        }
    }
    Ok(())
}

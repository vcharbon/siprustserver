//! Teardown: graceful call termination, per-leg destruction, and the BYE /
//! CANCEL builders (initial-INVITE CANCEL, transaction-scoped re-INVITE
//! CANCEL). The ADR-0022 unanswered-a-leg 503 does NOT live here — see
//! `crate::rules::invariants`.

use call::helpers::{
    record_termination, set_bye_disposition, set_leg_disposition, set_leg_state, Scope,
    TERMINATING_TIMEOUT_MS,
};
use call::{
    ByeDisposition, Call, LegDisposition, LegState, StackDialog, TerminationCause, TimerType,
};
use sip_message::generators::{
    self, GenerateInDialogRequestOpts, InDialogMethod, InviteClientTransactionHandle, RelayScope,
};
use sip_message::header::HeaderName;
use sip_message::parser::custom::CustomParser;
use sip_message::{hops, Method, SipHeader, SipMessage, SipParser};
use sip_txn::TxnKind;

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::invariants::GLOBAL_CALL_MACHINE;
use crate::rules::model::RuleContext;
use crate::rules::relay;

use super::select::{dialog_identity_tag, find_pending_dialog};
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Graceful teardown. For every leg not already resolved — `terminated`,
    /// any `byeDisposition` already set (the firing rule pre-marked it, e.g.
    /// `bye_received`), or `cancelling` (a `cancel-leg` CANCEL is already in
    /// flight) — issue the right teardown: confirmed → BYE + `bye_sent`;
    /// trying/early b-leg → CANCEL + `cancelled` + terminated; trying/early
    /// a-leg → `none` (the rule already sent the SIP reply) — and, when its
    /// INVITE carries its final (`invite_final_sent`), → terminated too (the
    /// answered leg is resolved, so the ADR-0022 unanswered-a-leg invariant
    /// stays a pure safety net). Then enter `terminating` and arm the safety
    /// timer.
    ///
    /// A terminating call makes no forward progress on its own clock: every
    /// per-leg `NoAnswer` entry and every service watchdog leaves the ledger
    /// and the driver, the ladders end, and every service machine is
    /// deactivated (its cursor removed) — so no fire reaches the terminating
    /// window and a reclaim cannot restore one into it. A rule that keeps its
    /// machine through the teardown re-installs its cursor with a `SetState`
    /// after this action.
    ///
    /// A relayed INVITE still pending on any dialog is answered first (RFC 3261
    /// §15.1.2: a UAS ending a dialog still responds to its pending requests,
    /// 487 recommended), its target CANCELled, so no INVITE transaction is
    /// left open behind the BYEs — a takeover copy is resident exactly while
    /// one is (ADR-0014).
    ///
    /// `source_leg_id` is intentionally *not* special-cased here: rules that
    /// consume a BYE/CANCEL pre-mark their source leg's disposition before
    /// emitting begin-termination, so the skip guard below leaves it untouched.
    ///
    /// `ended` is the termination record's `(cause, by_leg)`, written with
    /// this turn's clock when the call carries no record yet: the first
    /// termination names who ended the call.
    pub(super) fn begin_termination(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        reason: Option<&str>,
        ended: (TerminationCause, Option<String>),
    ) {
        *call = record_termination(call.clone(), self.now_ms, ended.0, ended.1);
        let pending_invites: Vec<(String, i64)> = std::iter::once(&call.a_leg)
            .chain(call.b_legs.iter())
            .flat_map(|leg| leg.dialogs.iter().map(move |d| (leg, d)))
            .flat_map(|(leg, d)| {
                d.ext
                    .inbound_pending_requests
                    .iter()
                    .filter(|p| p.method.eq_ignore_ascii_case("INVITE") && !p.cancelled)
                    .map(move |p| (leg.leg_id.clone(), p.outbound_cseq))
            })
            .collect();
        for (leg_id, outbound_cseq) in pending_invites {
            self.reject_pending_reinvite(
                call,
                fx,
                &leg_id,
                outbound_cseq,
                487,
                "Request Terminated",
            );
        }
        // RFC 3261 §16.6: a termination the peer ASKED for restates what it
        // said — the Q.850 cause (RFC 3326 §2), the charging correlation, the
        // end-to-end data — on the BYE/CANCEL minted for the other leg.
        let relayed = relayed_teardown_headers(ctx);
        let hops = relayed_teardown_hops(ctx);
        // RFC 3326: stamp the teardown cause on each BYE only when the firing
        // rule supplied a structured `SIP;cause=…` value (the
        // `promote18xPemTo200` diagnostic teardown). The CORE rules pass opaque
        // labels ("BYE"/"CANCEL"/"481"); those are not RFC 3326 values and are
        // not emitted on the wire.
        let reason_header = reason.filter(|r| r.trim_start().starts_with("SIP"));
        // a-leg ∪ b-legs, in that order.
        let legs: Vec<(String, LegState, LegDisposition, Option<ByeDisposition>, bool)> =
            std::iter::once(&call.a_leg)
                .chain(call.b_legs.iter())
                .map(|l| {
                    (
                        l.leg_id.clone(),
                        l.state,
                        l.disposition,
                        l.bye_disposition,
                        l.leg_id == call.a_leg.leg_id,
                    )
                })
                .collect();
        for (id, state, disposition, bye_disposition, is_a) in legs {
            // Skip legs already handled by the firing rule or already resolved.
            if state == LegState::Terminated {
                continue;
            }
            if bye_disposition.is_some() {
                continue;
            }
            if disposition == LegDisposition::Cancelling {
                continue;
            }
            match state {
                LegState::Confirmed => {
                    let e = if is_a {
                        self.bye_to_leg_a(call, reason_header, &relayed, hops)
                    } else {
                        self.bye_to_b_leg(call, &id, reason_header, &relayed, hops)
                    };
                    if let Some(e) = e {
                        fx.outbound.push(e);
                    }
                    *call = set_bye_disposition(call.clone(), &id, ByeDisposition::ByeSent);
                }
                LegState::Trying | LegState::Early => {
                    if is_a {
                        // a-leg trying/early: the rule already sent the SIP reply.
                        *call = set_bye_disposition(call.clone(), &id, ByeDisposition::None);
                        // Parity with the b-leg arm: an a-leg whose INVITE
                        // carries its final — this turn's reject, or the txn
                        // layer's autonomous 487 — is resolved; recorded
                        // `Terminated` so a later turn's `→ terminated` edge
                        // never reads a still-`Early` a-leg and re-answers it
                        // (ADR-0022). A leg with NO final yet stays
                        // Trying/Early — `answer_a_leg_if_unanswered` still
                        // owes that caller its 503.
                        if call.a_leg.invite_final_sent.is_some() {
                            self.reject_pending_non_invites(call, fx, &id);
                            *call = set_leg_state(call.clone(), &id, LegState::Terminated);
                        }
                    } else {
                        if let Some(e) = self.cancel_to_leg(call, &id, &relayed) {
                            fx.outbound.push(e);
                        }
                        *call = set_bye_disposition(call.clone(), &id, ByeDisposition::Cancelled);
                        // Same crossing-200 parity as `destroy_leg`: a ringing
                        // b-leg CANCELed by termination (e.g. `setup-timeout` +
                        // `BeginTermination` with no prior `CancelLeg`) must be
                        // `Cancelling` so a 200 racing the CANCEL is reaped by
                        // `cancel-200-crossing` rather than orphaning the callee.
                        *call = set_leg_disposition(call.clone(), &id, LegDisposition::Cancelling);
                        self.reject_pending_non_invites(call, fx, &id);
                        *call = set_leg_state(call.clone(), &id, LegState::Terminated);
                    }
                }
                LegState::Terminated => {}
            }
        }
        // Nothing awaits an answer and no service makes progress once the
        // call is terminating: every per-leg `NoAnswer` entry — including a
        // leg the loop skipped as already `Cancelling` — and every service
        // watchdog leaves the ledger and the driver.
        self.scrub_where(call, fx, |t| {
            matches!(t.timer_type, TimerType::NoAnswer | TimerType::Service { .. })
        });
        // Nothing is repeated into a terminating call either: every ladder
        // ends with the setup (a caller who CANCELled must not keep receiving
        // rungs), so a reclaim cannot restore one into it.
        self.retire(call, fx, Scope::Call);
        deactivate_service_machines(call);
        call.state = call::CallModelState::Terminating;
        self.schedule(call, fx, TimerType::TerminatingTimeout, TERMINATING_TIMEOUT_MS, None);
    }

    pub(super) fn destroy_leg(&self, call: &mut Call, fx: &mut HandlerEffects, leg_id: &str) {
        // A destroyed leg's ladders die with it (RFC 3262 §3): no rung
        // re-offers a torn-down leg's answer.
        self.retire(call, fx, Scope::Leg(leg_id));
        let state = call
            .b_legs
            .iter()
            .find(|l| l.leg_id == leg_id)
            .map(|l| l.state)
            .or_else(|| (call.a_leg.leg_id == leg_id).then_some(call.a_leg.state));
        match state {
            Some(LegState::Confirmed) => {
                if let Some(e) = self.bye_to_b_leg(call, leg_id, None, &[], None) {
                    fx.outbound.push(e);
                }
                *call = set_bye_disposition(call.clone(), leg_id, ByeDisposition::ByeSent);
            }
            Some(LegState::Trying) | Some(LegState::Early) => {
                if let Some(e) = self.cancel_to_leg(call, leg_id, &[]) {
                    fx.outbound.push(e);
                }
                *call = set_bye_disposition(call.clone(), leg_id, ByeDisposition::Cancelled);
                // Parity with the explicit `CancelLeg` path: mark the ringing leg
                // `Cancelling` so a 200 OK that crosses this internally-originated
                // CANCEL on the wire matches `cancel-200-crossing` and is
                // ACK+BYE'd. Without this the late-answering callee is orphaned in
                // a one-sided established call (the 200 matches no rule — its
                // `confirm-dialog` needs Trying/Early state, which we just left).
                *call = set_leg_disposition(call.clone(), leg_id, LegDisposition::Cancelling);
            }
            _ => {}
        }
        self.reject_pending_non_invites(call, fx, leg_id);
        *call = set_leg_state(call.clone(), leg_id, LegState::Terminated);
    }

    /// CANCEL a ringing b-leg and mark it `Cancelling`
    /// ([`crate::rules::model::RuleAction::CancelLeg`]).
    pub(super) fn cancel_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
    ) {
        // The cancelled fork's ladders die with it (RFC 3262 §3): no rung
        // re-offers a leg being cancelled.
        self.retire(call, fx, Scope::Leg(leg_id));
        if let Some(e) = self.cancel_to_leg(call, leg_id, &relayed_teardown_headers(ctx)) {
            fx.outbound.push(e);
        }
        *call = set_leg_disposition(call.clone(), leg_id, LegDisposition::Cancelling);
    }

    /// Bring `leg_id` to a terminal state
    /// ([`crate::rules::model::RuleAction::TerminateLeg`]).
    pub(super) fn terminate_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        bye_disposition: Option<ByeDisposition>,
    ) {
        // The terminal leg's ladders die with it (RFC 3262 §3), and the
        // requests relayed toward it are answered by this stack (§8.2.6).
        self.retire(call, fx, Scope::Leg(leg_id));
        self.reject_pending_non_invites(call, fx, leg_id);
        *call = set_leg_state(call.clone(), leg_id, LegState::Terminated);
        if let Some(bd) = bye_disposition {
            *call = set_bye_disposition(call.clone(), leg_id, bd);
        }
        // Settle an in-flight CANCEL: a leg that TerminateLeg brings to a
        // terminal state is no longer "cancel pending", so clear the
        // `Cancelling` disposition that held it unresolved (see
        // `call::helpers::leg_is_resolved`). This is the seam the `487`
        // (`resolve-cancel-response`) and the force-terminal reaper /
        // safety-timeout paths ride to let a deferred termination finally
        // finalize; the crossing-`200` path clears `Cancelling` earlier via
        // `ConfirmDialog` (→ `Bridged`) instead.
        let cancelling = call::helpers::find_leg(call, leg_id)
            .map(|l| l.disposition == LegDisposition::Cancelling)
            .unwrap_or(false);
        if cancelling {
            *call = set_leg_disposition(call.clone(), leg_id, LegDisposition::Rejected);
        }
    }

    fn bye_to_b_leg(
        &self,
        call: &Call,
        leg_id: &str,
        reason: Option<&str>,
        relayed: &[SipHeader],
        hops: Option<u32>,
    ) -> Option<OutboundSipEffect> {
        let leg = call.b_legs.iter().find(|l| l.leg_id == leg_id)?;
        let d = leg.dialogs.first()?;
        self.bye_on_dialog(
            &call.call_ref,
            leg_id,
            call.emergency == Some(true),
            &d.sip,
            reason,
            relayed,
            hops,
        )
    }

    fn bye_to_leg_a(
        &self,
        call: &Call,
        reason: Option<&str>,
        relayed: &[SipHeader],
        hops: Option<u32>,
    ) -> Option<OutboundSipEffect> {
        let d = call.a_leg.dialogs.first()?;
        self.bye_on_dialog(
            &call.call_ref,
            &call.a_leg.leg_id,
            call.emergency == Some(true),
            &d.sip,
            reason,
            relayed,
            hops,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn bye_on_dialog(
        &self,
        call_ref: &str,
        leg_id: &str,
        is_emergency: bool,
        sip: &StackDialog,
        reason: Option<&str>,
        relayed: &[SipHeader],
        hops: Option<u32>,
    ) -> Option<OutboundSipEffect> {
        if sip.remote_tag.is_empty() {
            return None; // not a confirmed dialog
        }
        let dialog = relay::to_gen_dialog(sip);
        let branch = self.id_gen.new_branch();
        let mut extra_headers: Vec<SipHeader> = reason
            .map(|r| {
                vec![SipHeader { name: "Reason".to_string().into(), value: r.to_string().into() }]
            })
            .unwrap_or_default();
        // The firing rule's own statement is the more specific one and stands;
        // every other name the releasing peer sent rides beside it, EVERY line
        // of it: a set-like header the peer split over several lines (`Allow`,
        // RFC 3261 §7.3.1) is one set, and keeping only its first line would
        // relay a one-token set the peer never stated.
        let stated = extra_headers.clone();
        for header in relayed {
            let name = HeaderName::from(header.name.as_str());
            if !stated.iter().any(|h| name.matches(&h.name)) {
                extra_headers.push(header.clone());
            }
        }
        // Per-dialog CSeq (§12.2.1.1): `generate_in_dialog_request` defaults to
        // this dialog's `local_cseq + 1`, the next sequence number within THIS
        // dialog (a forked sibling's CSeq is irrelevant — distinct dialog).
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, call_ref, leg_id, is_emergency, branch)),
            extra_headers,
            max_forwards: hops,
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Bye, &dialog, &opts);
        let dest = relay::target_dest(&dialog.remote_target);
        let (req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &dialog.route_set, res.request, dest);
        Some(OutboundSipEffect {
            body: OutboundBody::Request(req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("BYE → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance: Provenance::Authored,
        })
    }

    /// Transaction-scoped CANCEL of a relayed, still-pending **re-INVITE** on
    /// `leg_id`'s dialog (RFC 3261 §9.1). Unlike [`Self::cancel_to_leg`] /
    /// `CancelLeg` this touches NO leg state or disposition — the established
    /// dialog and the call stay up; only the renegotiation ends. The CANCEL is
    /// built from the dialog's cached `pending_invite_txn` handle, so it reuses
    /// the re-INVITE's branch, echoes its Route set (`generate_cancel`), and
    /// goes to the re-INVITE's cached wire destination — the same next-hop
    /// consistency as the initial-INVITE `cancel_to_leg` path. The matching
    /// pending-relay snapshot is marked `cancelled` so the target's eventual
    /// final (487, or a crossing 200) resolves locally instead of relaying.
    pub(super) fn cancel_pending_reinvite(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        outbound_cseq: i64,
    ) {
        let Some(cancel) = pending_reinvite_cancel(call, leg_id, outbound_cseq) else {
            return;
        };
        self.commit_pending_reinvite_cancel(call, fx, leg_id, outbound_cseq, cancel);
    }

    /// Put a built re-INVITE CANCEL on the wire and settle its books: the
    /// snapshot is marked `cancelled`, and the transaction's reliable
    /// provisionals end with the final the originator now holds — the
    /// transaction layer's 487 to a relayed CANCEL, or this stack's own
    /// reject (RFC 3262 §3).
    fn commit_pending_reinvite_cancel(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        outbound_cseq: i64,
        cancel: PendingReinviteCancel,
    ) {
        fx.outbound.push(cancel.effect);
        *call = call::helpers::cancel_pending_request(
            call.clone(),
            leg_id,
            &cancel.t_id,
            outbound_cseq,
        );
        self.retire(call, fx, Scope::Transaction { leg_id, cseq: outbound_cseq });
    }

    /// Answers every relayed non-INVITE request still pending toward `leg_id`
    /// where that leg goes `Terminated`: its target's answer relays no further,
    /// and RFC 3261 §8.2.6 owes the originator a final. A PRACK draws 200 — it
    /// named a provisional this stack showed under its own number (RFC 3262
    /// §3); anything else 481. The snapshot is dropped so a late answer from
    /// the target is never a second final (§17.2.1). A confirmed leg being
    /// BYEd keeps its relays; a pending INVITE is [`Self::reject_pending_reinvite`]'s.
    pub(super) fn reject_pending_non_invites(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
    ) {
        let leg = if call.a_leg.leg_id == leg_id {
            Some(&call.a_leg)
        } else {
            call.b_legs.iter().find(|l| l.leg_id == leg_id)
        };
        let Some(leg) = leg else { return };
        let pending: Vec<(String, call::PendingRequest)> = leg
            .dialogs
            .iter()
            .flat_map(|d| {
                let tag = dialog_identity_tag(leg_id, d);
                d.ext
                    .inbound_pending_requests
                    .iter()
                    .filter(|p| !p.method.eq_ignore_ascii_case("INVITE"))
                    .map(move |p| (tag.clone(), p.clone()))
            })
            .collect();
        let originator =
            call::helpers::get_peer(call, leg_id).unwrap_or(call.a_leg.leg_id.as_str()).to_string();
        for (identity_tag, p) in pending {
            *call = call::helpers::remove_pending_request(
                call.clone(),
                leg_id,
                &identity_tag,
                p.outbound_cseq,
            );
            // §18.2.2: the final goes to the originator's top Via sent-by.
            let Some(dest) =
                p.source_vias.first().and_then(|v| super::relay_response::via_sent_by(v))
            else {
                tracing::warn!(
                    call_ref = %call.call_ref,
                    leg_id = %leg_id,
                    method = %p.method,
                    "pending relay left unanswered — the originator's top Via names no destination"
                );
                continue;
            };
            let method = p.method.to_ascii_uppercase();
            let opts = super::relay_response::snapshot_response_opts(
                &p,
                &method,
                Vec::new(),
                None,
                Vec::new(),
                None,
            );
            let (status, reason) = if method == "PRACK" {
                (200, "OK")
            } else {
                (481, "Call/Transaction Does Not Exist")
            };
            fx.outbound.push(OutboundSipEffect {
                body: OutboundBody::Response(generators::generate_relayed_response(
                    status, reason, &opts,
                )),
                mode: OutboundTxnMode::ServerResponse,
                destination: dest,
                label: format!("{status} {method} → {originator}"),
                leg_id: Some(originator.clone()),
                provenance: Provenance::Authored,
            });
        }
    }

    /// [`Self::reject_pending_non_invites`] over every leg, for a termination
    /// that brings them all to `Terminated` at once (`TerminateCall`).
    pub(super) fn reject_all_pending_non_invites(&self, call: &mut Call, fx: &mut HandlerEffects) {
        let ids: Vec<String> = std::iter::once(call.a_leg.leg_id.clone())
            .chain(call.b_legs.iter().map(|l| l.leg_id.clone()))
            .collect();
        for id in ids {
            self.reject_pending_non_invites(call, fx, &id);
        }
    }

    /// End the relayed re-INVITE still pending on `leg_id`'s dialog under
    /// `outbound_cseq` on both faces, transaction-scoped (RFC 3261 §14.1 —
    /// the dialogs stay as they were): its originator is answered the locally
    /// authored `status` final, rebuilt from the pending snapshot the way a
    /// relayed final is (§8.2.6.2) and carrying nothing else — a failure
    /// states no body and no Contact — and the request toward the target is
    /// CANCELled as [`Self::cancel_pending_reinvite`] does. The two faces
    /// never disagree: the CANCEL is built before the final leaves, and where
    /// it cannot be built nothing is emitted, since a final the originator
    /// holds over a snapshot still live would let the target's own final be
    /// relayed as a second final on a completed transaction (RFC 3261
    /// §17.2.1). A snapshot already CANCELled has its originator's final from
    /// the transaction layer, so nothing is owed twice.
    pub(super) fn reject_pending_reinvite(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        outbound_cseq: i64,
        status: u16,
        reason: &str,
    ) {
        let Some((_, dialog)) = find_pending_dialog(call, leg_id, outbound_cseq) else {
            return;
        };
        let Some(pending) = call::helpers::find_pending_request(&dialog, outbound_cseq).cloned()
        else {
            return;
        };
        if pending.cancelled || !pending.method.eq_ignore_ascii_case("INVITE") {
            return;
        }
        let refused = |what: &str| {
            tracing::warn!(
                call_ref = %call.call_ref,
                leg_id = %leg_id,
                status,
                "re-INVITE reject dropped — {what}"
            );
        };
        // §18.2.2: the final goes to the originator's top Via sent-by; one no
        // reader accepts names no destination (see `relay_response`).
        let Some(dest) =
            pending.source_vias.first().and_then(|v| super::relay_response::via_sent_by(v))
        else {
            refused("the originator's top Via does not read, so it names no destination");
            return;
        };
        let Some(cancel) = pending_reinvite_cancel(call, leg_id, outbound_cseq) else {
            refused("the pending re-INVITE toward the target cannot be CANCELled");
            return;
        };
        let opts = super::relay_response::snapshot_response_opts(
            &pending,
            "INVITE",
            Vec::new(),
            None,
            Vec::new(),
            None,
        );
        let originator =
            call::helpers::get_peer(call, leg_id).unwrap_or(call.a_leg.leg_id.as_str()).to_string();
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Response(generators::generate_relayed_response(
                status, reason, &opts,
            )),
            mode: OutboundTxnMode::ServerResponse,
            destination: dest,
            label: format!("{status} INVITE → {originator}"),
            leg_id: Some(originator),
            provenance: Provenance::Authored,
        });
        self.commit_pending_reinvite_cancel(call, fx, leg_id, outbound_cseq, cancel);
    }

    pub(super) fn cancel_to_leg(
        &self,
        call: &Call,
        leg_id: &str,
        relayed: &[SipHeader],
    ) -> Option<OutboundSipEffect> {
        let leg = call.b_legs.iter().find(|l| l.leg_id == leg_id)?;
        let d = leg.dialogs.first()?;
        let handle = d.ext.pending_invite_txn.as_ref()?;
        let parsed = CustomParser::new().parse(&handle.original_invite).ok()?;
        let req = match parsed {
            SipMessage::Request(r) => r,
            _ => return None,
        };
        let cancel = generators::generate_cancel(
            &InviteClientTransactionHandle { original_invite: req },
            relayed,
        );
        // RFC 3261 §9.1: the CANCEL follows the INVITE's next hop, NOT `leg.source`
        // (the callee's advertised address). When the b-leg egresses through the
        // front proxy (`b2b_outbound_proxy`), the INVITE's wire destination — cached
        // on the txn handle at send — is the proxy; sending to `leg.source` would
        // bypass it, and the CANCEL would never reach the pending server txn the
        // proxy holds. The echoed Route set (above) keeps the CANCEL's path aligned
        // with the INVITE the whole way.
        Some(OutboundSipEffect {
            body: OutboundBody::Request(cancel),
            mode: OutboundTxnMode::Raw,
            destination: (handle.destination.host.clone(), handle.destination.port),
            label: format!("CANCEL → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance: Provenance::Authored,
        })
    }
}

/// Remove every service machine's cursor: a terminating call is the core
/// teardown's alone, so no machine-bound rule is a candidate on it. The
/// `global-call` projection stays — it is the lifecycle itself, re-projected
/// by `invariants::finalize`, which also restores the engine's own
/// projections (`transfer`, `relayFirst18x`) from their authoritative slices.
fn deactivate_service_machines(call: &mut Call) {
    call.sm_cursors.retain(|machine, _| *machine == GLOBAL_CALL_MACHINE);
}

/// A re-INVITE CANCEL ready to leave, with the identity of the dialog whose
/// snapshot it settles.
struct PendingReinviteCancel {
    t_id: String,
    effect: OutboundSipEffect,
}

/// Build the transaction-scoped CANCEL of the relayed re-INVITE pending on
/// `leg_id`'s dialog under `outbound_cseq` (RFC 3261 §9.1), from the dialog's
/// cached `pending_invite_txn` handle: the re-INVITE's own branch, Route set
/// and wire destination. `None` where no such transaction can be CANCELled —
/// no pending dialog, no cached handle, an unreadable one, or a handle naming
/// another CSeq than the snapshot (the glare guard forbids a second in-flight
/// INVITE on one dialog, and a mismatched transaction is never CANCELled).
fn pending_reinvite_cancel(
    call: &Call,
    leg_id: &str,
    outbound_cseq: i64,
) -> Option<PendingReinviteCancel> {
    let (t_id, dialog) = find_pending_dialog(call, leg_id, outbound_cseq)?;
    let handle = dialog.ext.pending_invite_txn.as_ref()?;
    let Ok(SipMessage::Request(req)) = CustomParser::new().parse(&handle.original_invite) else {
        return None;
    };
    if req.cseq().seq() as i64 != outbound_cseq {
        return None;
    }
    let cancel =
        generators::generate_cancel(&InviteClientTransactionHandle { original_invite: req }, &[]);
    Some(PendingReinviteCancel {
        t_id,
        effect: OutboundSipEffect {
            body: OutboundBody::Request(cancel),
            mode: OutboundTxnMode::Raw,
            destination: (handle.destination.host.clone(), handle.destination.port),
            label: format!("CANCEL re-INVITE → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance: Provenance::Authored,
        },
    })
}

/// What the peer said about the teardown it asked for, for the BYE or CANCEL
/// this stack mints toward the other leg (RFC 3261 §16.6). RFC 3326 §2 scopes
/// `Reason` to exactly these two methods, and the transaction layer already
/// answered the CANCEL, so its lines ride the event rather than a request.
/// Empty for a timer or a failure: those state a release of OURS, and nothing
/// is put in the peer's mouth. The minted request carries no body, so the
/// source's body metadata is withheld with it.
fn relayed_teardown_headers(ctx: &RuleContext) -> Vec<SipHeader> {
    let received = match ctx.request() {
        Some(request) if request.method() == Method::Bye => request.headers(),
        _ => ctx.cancelled_headers(),
    };
    generators::relayable_headers(received, RelayScope::request().without_source_body())
}

/// The hop count that teardown states (RFC 3261 §16.6 step 3): the releasing
/// peer's count less one where the peer ASKED for the teardown, and `None` —
/// the §8.1.1.6 default — for a release of OURS (a timer, a failure, a CANCEL
/// the transaction layer already answered), which starts a hop budget rather
/// than continuing one.
fn relayed_teardown_hops(ctx: &RuleContext) -> Option<u32> {
    match ctx.request() {
        Some(request) if request.method() == Method::Bye => {
            Some(hops::forwarded_max_forwards(request).value())
        }
        _ => None,
    }
}

/// Hard-terminate every leg and the call ([`crate::rules::model::RuleAction::TerminateCall`],
/// and the `CreateLeg` admission reject). No wire traffic — the firing rule owns
/// any final/BYE already sent. Writes the termination record under `cause`
/// and `by_leg` when the call carries none yet.
pub(super) fn terminate_all(
    call: &mut Call,
    now_ms: i64,
    cause: TerminationCause,
    by_leg: Option<String>,
) {
    *call = record_termination(call.clone(), now_ms, cause, by_leg);
    *call = set_leg_state(call.clone(), &call.a_leg.leg_id.clone(), LegState::Terminated);
    let ids: Vec<String> = call.b_legs.iter().map(|l| l.leg_id.clone()).collect();
    for id in ids {
        *call = set_leg_state(call.clone(), &id, LegState::Terminated);
    }
    call.state = call::CallModelState::Terminated;
}

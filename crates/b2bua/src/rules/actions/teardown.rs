//! Teardown: graceful call termination, per-leg destruction, and the BYE /
//! initial-INVITE CANCEL builders; the transaction-scoped re-INVITE CANCEL is
//! [`super::reinvite_cancel`]'s. The ADR-0022 unanswered-a-leg 503 does NOT
//! live here — see `crate::rules::invariants`.

use call::helpers::{
    abandon_leg, record_dialled_invite_final, record_termination, retire_early_dialogs, Scope,
    TERMINATING_TIMEOUT_MS,
};
use call::{
    Call, DialogId, HeldRequest, Leg, LegKind, LegState, TerminationCause, TimerKey, TimerType,
};
use sip_message::generators::{
    self, InviteClientTransactionHandle, RelayScope, RelaySituation, RelayedMessage,
};
use sip_message::header::{q850_cause_alone, readable_reasons, HeaderName, MediaType};
use sip_message::parser::custom::CustomParser;
use sip_message::{hops, Method, SipHeader, SipMessage, SipParser};

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::invariants::GLOBAL_CALL_MACHINE;
use crate::rules::relay;
use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::{LegEnd, RuleContext};
use b2bua_sdk::release_reason::CancelReason;

use super::{ActionExecutor, Owed, Settle};

impl ActionExecutor<'_> {
    /// Graceful teardown. Every leg still in play is ended by what its state
    /// reads: a confirmed leg is BYEd (its dialog `Ending`), a ringing b-leg
    /// CANCELled; a caller not yet answered keeps her INVITE for the firing
    /// rule's final or the ADR-0022 unanswered-caller final. A leg that reads
    /// `Terminated` — the firing rule ended it, its CANCEL or BYE already
    /// left — is left alone. Then enter `terminating` and arm the safety
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
    /// consume a BYE end their source leg (`TerminateLeg`) before emitting
    /// begin-termination, so it reads `Terminated` here.
    ///
    /// `ended` is the termination record's `(cause, by_leg)`, written with
    /// this turn's clock when the call carries no record yet: the first
    /// termination names who ended the call. A tentative stated set gives way
    /// to the one it replaced before anything is sent.
    ///
    /// A BYE states, first found: the firing rule's `SIP;cause=…` value, the
    /// releasing peer's own `Reason` where the peer asked for the teardown,
    /// else the deployment's `own_release_reason` (RFC 3326 §2: each is its
    /// sender's statement). No `Reason` reaches a media leg.
    pub(super) fn begin_termination(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        reason: Option<&str>,
        ended: (TerminationCause, Option<String>),
    ) {
        *call = record_termination(call.clone(), self.now_ms, ended.0, ended.1);
        crate::rules::stated_headers::withdraw_tentative(call);
        let pending_invites: Vec<(String, i64)> = std::iter::once(&call.a_leg)
            .chain(call.b_legs.iter())
            .flat_map(|leg| leg.dialogs.iter().map(move |d| (leg, d)))
            .flat_map(|(leg, d)| {
                d.inbound_pending_requests
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
        // end-to-end data and body — on the BYE/CANCEL minted for the other leg.
        let relayed = |method: &Method, leg_id: &str| {
            relayed_teardown(ctx, RelaySituation::request(method, relay::toward_leg(leg_id)))
        };
        let hops = relayed_teardown_hops(ctx);
        // RFC 3326: the firing rule's teardown cause rides each BYE only when
        // it is a structured `SIP;cause=…` value (the `promote18xPemTo200`
        // diagnostic teardown). The CORE rules pass opaque labels
        // ("BYE"/"CANCEL"/"481"); those are not RFC 3326 values and are not
        // emitted on the wire.
        let reason_header = reason.filter(|r| r.trim_start().starts_with("SIP"));
        // a-leg ∪ b-legs, in that order.
        let legs: Vec<(String, LegState, bool, bool)> = std::iter::once(&call.a_leg)
            .chain(call.b_legs.iter())
            .map(|l| {
                (l.leg_id.clone(), l.state(), l.invite_ringing(), l.leg_id == call.a_leg.leg_id)
            })
            .collect();
        let mut ended_leg_ringing = false;
        for (id, state, ringing, is_a) in legs {
            ended_leg_ringing |= ringing && state == LegState::Terminated;
            match state {
                LegState::Confirmed => {
                    let relayed = relayed(&Method::Bye, &id);
                    let reason = reason_header.or_else(|| own_release_reason(ctx));
                    self.send_bye(call, fx, &id, reason, relayed, hops);
                    super::dialog_end::end_dialogs(call, ctx, &id, LegEnd::OurByeSent);
                }
                // The caller's INVITE is answered by the firing rule, or by
                // the unanswered-caller final as the call ends (ADR-0022);
                // once it carries its final — this turn's reject, or the txn
                // layer's autonomous 487 — the requests relayed toward her
                // are answered too.
                _ if is_a => {
                    if call.a_leg.invite_final_sent.is_some() {
                        self.reject_pending_non_invites(call, fx, &id);
                    }
                }
                // A b-leg whose INVITE still rings is CANCELled (§9.1), an
                // early dialog its peer ended aside: a 200 crossing the
                // CANCEL is reaped by `cancel-200-crossing`.
                _ if ringing => {
                    let relayed = relayed(&Method::Cancel, &id);
                    self.send_cancel(call, fx, &id, &relayed.headers);
                    self.reject_pending_non_invites(call, fx, &id);
                }
                // Already ended or being ended: its CANCEL or BYE left, its
                // INVITE took its final.
                _ => {}
            }
        }
        // A leg a fork's early BYE ended holds the call only for its INVITE's
        // 487: the caller is answered now, owed nothing from that CANCEL.
        if ended_leg_ringing {
            crate::rules::invariants::answer_unanswered_caller(
                call,
                fx,
                self.now_ms,
                &self.config.minted_final_advertisement,
            );
        }
        // Nothing awaits an answer, no service makes progress and nothing is
        // repeated once the call is terminating: the call's scope retires —
        // every leg's timers, every service watchdog and every ladder (a
        // caller who CANCELled must not keep receiving rungs), so a reclaim
        // cannot restore one into it.
        self.retire(call, fx, Scope::Call);
        deactivate_service_machines(call);
        call.state = call::CallModelState::Terminating;
        self.schedule(
            call,
            fx,
            TimerKey::call(TimerType::TerminatingTimeout),
            TERMINATING_TIMEOUT_MS,
        );
    }

    /// End `leg_id` ([`b2bua_sdk::model::RuleAction::DestroyLeg`]): BYE a
    /// confirmed dialog, CANCEL a pending INVITE, each carrying `stated`.
    pub(super) fn destroy_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
        stated: &[(String, String)],
    ) {
        // A destroyed leg's ladders die with it (RFC 3262 §3): no rung
        // re-offers a torn-down leg's answer.
        self.retire(call, fx, Scope::Leg(leg_id));
        let stated: Vec<SipHeader> = stated
            .iter()
            .map(|(name, value)| SipHeader {
                name: name.clone().into(),
                value: value.clone().into(),
            })
            .collect();
        let leg = call::helpers::find_leg(call, leg_id);
        let state = leg.map(|l| l.state());
        let ringing = leg.is_some_and(|l| l.invite_ringing());
        match state {
            Some(LegState::Confirmed) => {
                // A release no peer asked for states the deployment's cause
                // where the rule states none (RFC 3326 §2).
                let stated_reason = stated.iter().any(|h| HeaderName::Reason.matches(&h.name));
                let own = own_release_reason(ctx).filter(|_| !stated_reason);
                self.send_bye(call, fx, leg_id, own, Relayed::headers(stated), None);
                super::dialog_end::end_dialogs(call, ctx, leg_id, LegEnd::OurByeSent);
            }
            // The CANCEL leaves the leg `Cancelling`: a 200 OK crossing it
            // matches `cancel-200-crossing` and is ACK+BYE'd.
            _ if ringing => {
                self.send_cancel(call, fx, leg_id, &stated);
            }
            _ => {}
        }
        self.reject_pending_non_invites(call, fx, leg_id);
        *call = call::helpers::update_leg(call.clone(), leg_id, settle_ended_leg);
    }

    /// CANCEL a ringing b-leg, which then reads `Cancelling`
    /// ([`b2bua_sdk::model::RuleAction::CancelLeg`]).
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
        let situation = RelaySituation::request(&Method::Cancel, relay::toward_leg(leg_id));
        self.send_cancel(call, fx, leg_id, &relayed_teardown(ctx, situation).headers);
    }

    /// End `leg_id` as `end` records
    /// ([`b2bua_sdk::model::RuleAction::TerminateLeg`]).
    pub(super) fn terminate_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
        end: LegEnd,
    ) {
        // The terminal leg's ladders die with it (RFC 3262 §3), and the
        // requests relayed toward it are answered by this stack (§8.2.6).
        self.retire(call, fx, Scope::Leg(leg_id));
        self.reject_pending_non_invites(call, fx, leg_id);
        match end {
            LegEnd::InviteRejected => match rejecting_final(ctx, leg_id) {
                Some((cseq, status)) => {
                    *call = record_dialled_invite_final(call.clone(), leg_id, cseq, status);
                }
                None => *call = call::helpers::update_leg(call.clone(), leg_id, abandon_leg),
            },
            LegEnd::Abandon => *call = call::helpers::update_leg(call.clone(), leg_id, abandon_leg),
            LegEnd::Settle => {}
            LegEnd::PeerByeAnswered
            | LegEnd::OurByeSent
            | LegEnd::OurByeAnswered
            | LegEnd::OurByeTimedOut => super::dialog_end::end_dialogs(call, ctx, leg_id, end),
        }
        *call = call::helpers::update_leg(call.clone(), leg_id, settle_ended_leg);
    }

    /// Answers every relayed non-INVITE request still pending toward `leg_id`
    /// where that leg goes `Terminated`: its target's answer relays no further,
    /// and RFC 3261 §8.2.6 owes the originator a final. A PRACK draws 200 — it
    /// named a provisional this stack showed under its own number (RFC 3262
    /// §3); anything else 481 — the relays of early dialogs the answer retired
    /// included. The snapshot is dropped so a late answer from the target is
    /// never a second final (§17.2.1). A confirmed leg being
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
        let mut pending: Vec<(Option<DialogId>, call::PendingRequest)> = leg
            .dialogs
            .iter()
            .flat_map(|d| {
                let id = d.id();
                d.inbound_pending_requests
                    .iter()
                    .filter(|p| !p.method.eq_ignore_ascii_case("INVITE"))
                    .map(move |p| (Some(id.clone()), p.clone()))
            })
            .collect();
        // The relays of early dialogs the answer retired end with the leg too.
        let mut retired = Vec::new();
        *call = call::helpers::update_leg(call.clone(), leg_id, |l| {
            retired = call::helpers::take_retired_relays(l);
        });
        pending.extend(
            retired
                .into_iter()
                .filter(|(_, p)| !p.method.eq_ignore_ascii_case("INVITE"))
                .map(|(_, p)| (None, p)),
        );
        let originator =
            call::helpers::get_peer(call, leg_id).unwrap_or(call.a_leg.leg_id.as_str()).to_string();
        for (id, p) in pending {
            if let Some(id) = id {
                *call = call::helpers::remove_pending_request(
                    call.clone(),
                    leg_id,
                    &id,
                    p.outbound_cseq,
                );
            }
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

    /// CANCEL `leg_id`'s INVITE carrying `relayed` (RFC 3261 §9.1): the PRACKs
    /// its responders are owed leave first — the CANCEL does not end the
    /// INVITE transaction, and the party they were shown to will not PRACK
    /// them now (RFC 3262 §4) — then the CANCEL; the leg reads `Cancelling`
    /// until its INVITE's final, and each early dialog of that INVITE is
    /// ending. A leg holding no handle of its INVITE is recorded cancelled all
    /// the same.
    fn send_cancel(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        relayed: &[SipHeader],
    ) {
        self.settle_owed_pracks(call, fx, Owed::Leg(leg_id), Settle::Ending);
        let media = call.b_legs.iter().find(|l| l.leg_id == leg_id).is_some_and(is_media);
        let stated = if media { without_reason(relayed) } else { relayed.to_vec() };
        fx.outbound.extend(self.cancel_to_leg(call, leg_id, &stated));
        let stated = stated.iter().map(|h| (h.name.as_str().into(), h.value.as_str().into()));
        *call = call::helpers::record_cancel_sent(call.clone(), leg_id, stated.collect());
    }

    fn cancel_to_leg(
        &self,
        call: &Call,
        leg_id: &str,
        relayed: &[SipHeader],
    ) -> Option<OutboundSipEffect> {
        let leg = call.b_legs.iter().find(|l| l.leg_id == leg_id)?;
        let handle = leg.outbound_invite.as_ref()?.txn.as_ref()?;
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

/// The deployment's `own_release_reason` for a teardown in this turn: `None`
/// where a peer asked for it (its BYE or CANCEL is the turn's event), whose
/// own `Reason` is the one relayed.
fn own_release_reason<'a>(ctx: &RuleContext<'a>) -> Option<&'a str> {
    let asked_by_peer = matches!(ctx.event, CallEvent::Cancelled { .. })
        || ctx.request().is_some_and(|r| r.method() == Method::Bye);
    ctx.config.own_release_reason.as_deref().filter(|_| !asked_by_peer)
}

/// A media leg is the dialling service's resource, not a party a release
/// cause informs: no `Reason` reaches it, relayed or the stack's own.
pub(super) fn is_media(leg: &call::Leg) -> bool {
    call::helpers::leg_kind(leg) == LegKind::Media
}

pub(super) fn without_reason(headers: &[SipHeader]) -> Vec<SipHeader> {
    headers.iter().filter(|h| !HeaderName::Reason.matches(&h.name)).cloned().collect()
}

/// Remove every service machine's cursor: a terminating call is the core
/// teardown's alone, so no machine-bound rule is a candidate on it. The
/// `global-call` projection stays — it is the lifecycle itself, re-projected
/// by `invariants::finalize`, which also restores the engine's own
/// projections (`transfer`, `relayFirst18x`) from their authoritative slices.
fn deactivate_service_machines(call: &mut Call) {
    call.sm_cursors.retain(|machine, _| *machine == GLOBAL_CALL_MACHINE);
}

/// What a minted teardown request carries of the peer's: its end-to-end
/// headers and, on a BYE minted from the peer's BYE, its body.
pub(super) struct Relayed {
    pub(super) headers: Vec<SipHeader>,
    pub(super) body: Vec<u8>,
    pub(super) content_type: Option<MediaType>,
}

impl Relayed {
    /// Nothing of a peer's: a request this stack mints on its own account.
    pub(super) fn none() -> Self {
        Self::headers(Vec::new())
    }

    /// Headers alone, no body.
    pub(super) fn headers(headers: Vec<SipHeader>) -> Self {
        Self { headers, body: Vec::new(), content_type: None }
    }

    /// The BYE carrying these and `reason`, `hops`, held as what it says.
    pub(super) fn held_bye(self, reason: Option<&str>, hops: Option<u32>) -> HeldRequest {
        HeldRequest::Bye {
            reason: reason.map(str::to_string),
            headers: self
                .headers
                .iter()
                .map(|h| (h.name.to_string(), h.value.to_string()))
                .collect(),
            body: self.body,
            content_type: self.content_type.map(|t| t.to_string()),
            max_forwards: hops,
        }
    }
}

/// What the peer said about the teardown it asked for, for the BYE or CANCEL
/// this stack mints toward the other leg (RFC 3261 §16.6). RFC 3326 §2 scopes
/// `Reason` to exactly these two methods, and the transaction layer already
/// answered the CANCEL, so its lines ride the event rather than a request.
/// Empty for a timer or a failure: those state a release of OURS, and nothing
/// is put in the peer's mouth. A minted request of another method than the
/// peer's (a CANCEL for an early BYE, §9.1, or a BYE for a CANCEL) relays
/// nothing of it but `Reason`. A `Timestamp` the peer stated is restated with
/// this turn's clock (RFC 3261 §20.38). A minted CANCEL restates that `Reason` as the
/// deployment's `relayed_cancel_reason` says. A BYE minted from the peer's BYE
/// carries its body verbatim with every header describing it (§20.11); any
/// other minted request carries no body, so the source's body metadata is
/// withheld. `situation` names the minted request for the relay policy.
fn relayed_teardown(ctx: &RuleContext, situation: RelaySituation<'_>) -> Relayed {
    let bye = ctx.request().filter(|request| request.method() == Method::Bye);
    let (received, method) = match bye {
        Some(request) => (request.headers(), Method::Bye),
        None => (ctx.cancelled_headers(), Method::Cancel),
    };
    let same_method = situation.message == RelayedMessage::Request(&method);
    let carried = bye.filter(|request| same_method && !request.body().is_empty());
    let source_body = match carried {
        Some(_) => RelayScope::request(),
        None => RelayScope::request().without_source_body(),
    };
    let stamp = generators::timestamp_value(ctx.now_ms);
    let scope = ctx.config.relay_scope(source_body, situation).stamped(Some(&stamp));
    let mut headers = generators::relayable_headers(received, scope);
    if !same_method {
        headers.retain(|h| HeaderName::Reason.matches(&h.name));
    }
    if situation.message == RelayedMessage::Request(&Method::Cancel) {
        restate_cancel_reason(&mut headers, ctx.config.relayed_cancel_reason);
    }
    match carried {
        Some(request) => Relayed {
            headers,
            body: request.body().to_vec(),
            content_type: request.raw(HeaderName::ContentType).next().and_then(relay::media_type),
        },
        None => Relayed::headers(headers),
    }
}

/// `relayed`'s `Reason` lines as `how` restates them on a minted CANCEL:
/// unchanged, or a leading Q.850 value's cause alone (none where the first
/// value is not a Q.850 one stating a cause).
fn restate_cancel_reason(relayed: &mut Vec<SipHeader>, how: CancelReason) {
    if how == CancelReason::Verbatim {
        return;
    }
    let reasons = readable_reasons(
        relayed.iter().filter(|h| HeaderName::Reason.matches(&h.name)).map(|h| h.value.clone()),
    );
    relayed.retain(|h| !HeaderName::Reason.matches(&h.name));
    if let Some(value) = q850_cause_alone(&reasons) {
        relayed.push(SipHeader { name: "Reason".to_string().into(), value: value.into() });
    }
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

/// Hard-terminate every leg and the call
/// ([`b2bua_sdk::model::RuleAction::TerminateCall`]): each leg is abandoned
/// ([`abandon_leg`]). No wire traffic — the firing rule owns any final/BYE
/// already sent. Writes the termination record under `cause`
/// and `by_leg` when the call carries none yet.
pub(super) fn terminate_all(
    call: &mut Call,
    now_ms: i64,
    cause: TerminationCause,
    by_leg: Option<String>,
) {
    *call = record_termination(call.clone(), now_ms, cause, by_leg);
    for leg in std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()) {
        abandon_leg(leg);
        settle_ended_leg(leg);
    }
    call.state = call::CallModelState::Terminated;
}

/// The non-2xx final the turn's event carries to `leg_id`'s INVITE, as
/// `(CSeq number, status)`.
fn rejecting_final(ctx: &RuleContext, leg_id: &str) -> Option<(i64, u16)> {
    ctx.response()
        .filter(|r| ctx.source_leg_id == leg_id && r.cseq().method() == Method::Invite)
        .filter(|r| r.status() >= 300)
        .map(|r| (i64::from(r.cseq().seq()), r.status()))
}

/// A leg that reads `Terminated` lets go of the early dialogs its INVITE left
/// open: each retires with its open relays (RFC 3261 §12.1.2); a dialog whose
/// CANCEL is in flight still ends on the INVITE's final.
fn settle_ended_leg(leg: &mut Leg) {
    if leg.state() == LegState::Terminated {
        retire_early_dialogs(leg);
    }
}

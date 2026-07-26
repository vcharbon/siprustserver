//! The origination recipes — every request this endpoint SENDS: the initial
//! INVITE (plain or template), in-dialog INFO/MESSAGE, delayed-offer
//! re-INVITE, UPDATE, OPTIONS keepalive pings, templated in-dialog/early
//! requests, and the §14.1 / RFC 3311 §5.2 glare-retry wait arms. What happens
//! to their RESPONSES lives in [`super::response`].

use std::time::Duration;

use tokio::time::Instant;

use sip_message::generators::InDialogMethod;
use sip_message::{EmitOpts, MessageTemplate};

use super::answer::discharge_on_teardown;
use super::ledger::{ObligationKey, ObligationKind};
use super::runner::ActorState;
use super::state::Observation;
use crate::StepError;

/// Originate a plain in-dialog request (INFO/MESSAGE) carrying an optional
/// typed body + extra headers on the confirmed dialog, opening the method's
/// ledger obligation — its 2xx alone closes it (the reactor observes it).
pub(super) async fn originate_in_dialog(
    st: &mut ActorState<'_>,
    method: InDialogMethod,
    content_type: Option<String>,
    body: Option<Vec<u8>>,
    headers: Vec<(String, String)>,
) -> Result<(), StepError> {
    let now = Instant::now();
    let (key, dialog_clone) = {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| {
            StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: format!("{} goal with no confirmed dialog", method.as_str()),
            }
        })?;
        let mut req = dialog.send_request(method);
        match (body, content_type) {
            // A typed body rides `with_body` (Content-Type + Content-Length);
            // an untyped body still ships under a generic type so
            // Content-Length is emitted.
            (Some(bytes), ct) => {
                req = req.with_body(ct.as_deref().unwrap_or("application/octet-stream"), bytes);
            }
            // A content-type with no body: emit it as a header (`with_body`
            // only stamps Content-Type for a non-empty body).
            (None, Some(ct)) => req = req.with_header("Content-Type", &ct),
            (None, None) => {}
        }
        for (name, value) in &headers {
            req = req.with_header(name, value);
        }
        // The 2xx arrives through the reactor (recv_any); the returned
        // transaction handle is not awaited on here.
        let (_txn, request) = req.try_send_with_request().await?;
        // INFO/MESSAGE map to InDialog; any other method routed through this
        // goal opens under its own kind so its final still matches.
        let kind =
            ObligationKind::from_cseq_method(method.as_str()).unwrap_or(ObligationKind::InDialog);
        (ObligationKey::new(st.role, kind, request.cseq.seq), dialog.clone())
    };
    st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
    st.obs.record(
        Observation::RequestSent { key, detail: format!("{} awaiting 2xx", method.as_str()) },
        now,
    );
    Ok(())
}

/// Originate the initial INVITE (plain or template-driven) and register the
/// caller bookkeeping — the shared realization of `Invite`/`InviteTemplate`.
pub(super) async fn originate_initial_invite(
    st: &mut ActorState<'_>,
    callee: &'static str,
    plan: Option<crate::realcall::InvitePlan>,
    template: Option<(MessageTemplate, EmitOpts)>,
) -> Result<(), StepError> {
    let target = st.invite_targets.get(callee).cloned().ok_or_else(|| {
        StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: format!("Invite goal has no bound target {callee:?}"),
        }
    })?;
    let mut builder = st.agent.invite(&target);
    if let Some(offer) = st.media.offer_sdp() {
        builder = builder.with_sdp(offer);
    }
    // A declared delayed automatic (ADR-0024 §6): hold this INVITE's automatic
    // ACK-to-2xx for the declared duration (the reactor's `inv.ack()` honours it).
    if let Some(d) = st.delayed {
        builder = builder.delayed_ack(Duration::from_millis(d.delay_ms));
    }
    if let Some((tmpl, opts)) = &template {
        builder = builder.template(tmpl, *opts);
    }
    builder = match &plan {
        // The owned realization of `CallEnv::outgoing_invite` (route,
        // correlation stamp, egress rewrite) — the load/SUT path.
        Some(plan) => plan.apply(builder),
        // Plan-less: the toy-call path (optional bare proxy hop).
        None => match st.via {
            Some(via) => builder.through(via),
            None => builder,
        },
    };
    // A caller advertising `Supported: 100rel` (on the plan or a frozen
    // template header) awaits a reliable `183` — the `expected` of an
    // incidental shed/reject WrongStatus (linear `establish_100rel` parity).
    let advertises_100rel = plan.as_ref().is_some_and(|p| {
        p.headers.iter().any(|(n, v)| {
            n.eq_ignore_ascii_case("supported") && v.to_ascii_lowercase().contains("100rel")
        })
    }) || template.as_ref().is_some_and(|(t, _)| {
        t.headers().iter().any(|h| {
            sip_message::message_helpers::name_matches("Supported", &h.name)
                && h.value.to_ascii_lowercase().contains("100rel")
        })
    });
    if advertises_100rel {
        st.expected_provisional = 183;
    }
    let call = builder.send().await;
    st.scope.set_early(call.cancel_handle());
    st.dialogs.pending_invite = Some(call);
    // The caller APPEARS the moment she originates — so `all_terminated`
    // cannot fire (and the runner exit) before she has processed her own
    // INVITE's final. Without this, a callee that terminates immediately
    // (the `invite_reject` 486) can make the obs "all terminated" while
    // the caller's leg has not yet recorded a fact, so the runner exits
    // before she ACKs the reject (RFC 3261 §17.1.1.3). Monotone: a later
    // provisional/answer only advances the phase.
    st.obs.record(Observation::LegEarly { leg: st.role }, Instant::now());
    Ok(())
}

/// Send a templated request on the confirmed dialog (or, `early`, on the
/// still-pending INVITE's early dialog — RFC 3311 §5.1) and open the method's
/// ledger obligation, mirroring the semantic goal's bookkeeping (re-INVITE →
/// outstanding offer + retained txn for the 491 hop-ACK; UPDATE → outstanding
/// offer; BYE → teardown discharge first).
pub(super) async fn send_request_template(
    st: &mut ActorState<'_>,
    template: &MessageTemplate,
    opts: EmitOpts,
    early: bool,
) -> Result<(), StepError> {
    let now = Instant::now();
    let method = template
        .method()
        .and_then(|m| InDialogMethod::try_from(m).ok())
        .ok_or_else(|| StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "RequestTemplate requires an in-dialog request template".to_string(),
        })?;
    if method == InDialogMethod::Bye && !early {
        // A hangup subsumes this leg's pending in-dialog acks (§15) exactly
        // like the semantic `Bye` goal.
        discharge_on_teardown(st, now);
    }
    let (txn, req, dialog_clone) = if early {
        let inv = st.dialogs.pending_invite.as_mut().ok_or_else(|| {
            StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: "RequestTemplate{early} with no pending early dialog".to_string(),
            }
        })?;
        let tag = inv.early_remote_tag().to_string();
        let mut b = inv.send_request(method).template(template, opts);
        if !tag.is_empty() {
            b = b.with_to_tag(&tag);
        }
        let (txn, req) = b.try_send_with_request().await?;
        (txn, req, None)
    } else {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| {
            StepError::UnexpectedKind {
                who: st.role.to_string(),
                detail: "RequestTemplate with no confirmed dialog".to_string(),
            }
        })?;
        let (txn, req) =
            dialog.send_request(method).template(template, opts).try_send_with_request().await?;
        (txn, req, Some(dialog.clone()))
    };
    let cseq = req.cseq.seq;
    let kind = ObligationKind::from_cseq_method(method.as_str()).unwrap_or(ObligationKind::InDialog);
    match method {
        InDialogMethod::Invite => {
            st.sent_reinvites.insert(cseq);
            st.sent_reinvite_txns.insert(cseq, txn);
        }
        InDialogMethod::Update => {
            st.sent_updates.insert(cseq);
        }
        _ => {}
    }
    if let Some(d) = dialog_clone {
        st.scope.set_confirmed(d); // refresh so a teardown BYE stays valid
    }
    st.obs.record(
        Observation::RequestSent {
            key: ObligationKey::new(st.role, kind, cseq),
            detail: format!("templated {} awaiting final", method.as_str()),
        },
        now,
    );
    Ok(())
}

/// Originate ONE delayed-offer (bodyless) re-INVITE on the confirmed dialog and
/// register its bookkeeping: open the `ReInvite` obligation keyed on its CSeq,
/// track the CSeq in `sent_reinvites` (the 2xx-ACK / completion path) and retain
/// the client transaction in `sent_reinvite_txns` so a NON-2xx final (a 491
/// glare reject, C4/S5) can be hop-ACKed. Shared by [`GoalStep::Reinvite`] and
/// the §14.1 glare RETRY arm — so a retried re-INVITE is byte-identical to the
/// first (fresh CSeq, same delayed-offer shape).
pub(super) async fn originate_reinvite(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let now = Instant::now();
    let (key, dialog_clone, txn) = {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "Reinvite with no confirmed dialog".to_string(),
        })?;
        let txn = dialog.request(InDialogMethod::Invite, None).await;
        let cseq = dialog.local_cseq();
        (ObligationKey::new(st.role, ObligationKind::ReInvite, cseq), dialog.clone(), txn)
    };
    st.sent_reinvites.insert(key.cseq);
    st.sent_reinvite_txns.insert(key.cseq, txn);
    st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
    st.obs.record(
        Observation::RequestSent { key, detail: "re-INVITE awaiting 2xx".to_string() },
        now,
    );
    Ok(())
}

/// Park until the pending §14.1 glare retry is due (or forever if none).
pub(super) async fn wait_reinvite_retry(retry: &Option<Instant>) {
    match retry {
        Some(at) => tokio::time::sleep_until(*at).await,
        None => std::future::pending().await,
    }
}

/// Originate ONE in-dialog UPDATE (RFC 3311) carrying this leg's offer, opening
/// the `Update` obligation and marking the offer OUTSTANDING (`sent_updates`).
/// Shared by [`GoalStep::Update`] and the S6 collision RETRY arm.
pub(super) async fn originate_update(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let now = Instant::now();
    let offer = st.media.offer_sdp().unwrap_or(crate::OFFER_SDP);
    let (key, dialog_clone) = {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "Update with no confirmed dialog".to_string(),
        })?;
        let _upd =
            dialog.send_request(InDialogMethod::Update).with_sdp(offer).try_send().await?;
        let cseq = dialog.local_cseq();
        (ObligationKey::new(st.role, ObligationKind::Update, cseq), dialog.clone())
    };
    st.sent_updates.insert(key.cseq);
    st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
    st.obs
        .record(Observation::RequestSent { key, detail: "update awaiting 200".to_string() }, now);
    Ok(())
}

/// Park until the pending S6 UPDATE-collision retry is due (or forever if none).
pub(super) async fn wait_update_retry(retry: &Option<Instant>) {
    match retry {
        Some(at) => tokio::time::sleep_until(*at).await,
        None => std::future::pending().await,
    }
}

/// Send ONE in-dialog OPTIONS keepalive ping on the confirmed dialog and read
/// its 200 inline — the reactor is parked on the goal arm during the ping, so
/// the 200 is consumed here (mirrors the linear `options_hold`/`long_call`
/// pings). The FIRST ping stamps the `keepalive_ack` feed exactly once.
pub(super) async fn ping_options_once(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let (mut opt, dialog_clone) = {
        let dialog = st.dialogs.confirmed.as_mut().ok_or_else(|| StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "Options goal with no confirmed dialog".to_string(),
        })?;
        let opt = dialog.request(InDialogMethod::Options, None).await;
        (opt, dialog.clone())
    };
    st.scope.set_confirmed(dialog_clone); // refresh so a teardown BYE stays valid
    opt.try_expect(200).await?;
    if !st.saw_options_200 {
        st.saw_options_200 = true;
        st.feed.on_options_ok.stamp(st.ctx);
    }
    Ok(())
}

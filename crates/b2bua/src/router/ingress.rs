//! The pre-dispatch pipeline: every event enters here once. Data-path metrics,
//! per-peer timeout attribution, the out-of-dialog OPTIONS health responder,
//! resolution (an event resolving to no call goes to [`super::unroutable`]), a
//! new INVITE's router rungs of the admission ladder ([`super::admit`]), then
//! the offer of the event's [`Turn`] to its call's queue under its dispatch
//! class ([`crate::dispatch::class`]). The router acts on the offer: an
//! admitted new INVITE spends its CPS token once its turn is queued, the reaper
//! hears a call past its lifetime cap or at its overflow ceiling, and a
//! discarded request is paid what it is owed ([`super::owed`]).

use std::sync::Arc;

use sip_message::SipMessage;

use super::admit;
use super::owed::OwedAnswer;
use super::peer_metrics::classify_b2bua_peer;
use super::resolve::{initial_invite_copy, replica_takeover, resolve, InitialInvite};
use super::responses::build_options_health_response;
use super::turn::Turn;
use super::unroutable::Lookup;
use super::RouterCtx;
use crate::dispatch::{Discarded, DispatchClass, Outcome};
use b2bua_sdk::event::CallEvent;

/// Take `event` in; `matched_client_txn`: a response it carries answered a
/// client transaction the transaction layer holds.
pub(super) async fn on_event(ctx: &Arc<RouterCtx>, event: CallEvent, matched_client_txn: bool) {
    // Per-method / per-(method,code) data-path counters. Every inbound SIP
    // message lands here once, so this is the single chokepoint to meter them.
    if let CallEvent::Sip { message, .. } = &event {
        match message.as_ref() {
            SipMessage::Request(req) => ctx.metrics.record_request(req.method().as_str()),
            SipMessage::Response(resp) => {
                ctx.metrics.record_response(resp.cseq().method().as_str(), resp.status())
            }
        }
    }

    // Per-peer timeout attribution (observability only;
    // b2bua_peer_failures_total{peer,scope,kind}). A client transaction gave up
    // with no final response: split response_timeout (Timer B/F) vs
    // transaction_timeout (the long out-of-dialog INVITE backstop) by the
    // forwarded `timeout_kind`, and classify the peer internal/external against
    // the configured outbound proxy. `destination == None` (legacy txns) is
    // skipped rather than fabricated.
    if let CallEvent::Timeout { destination: Some(dest), timeout_kind, .. } = &event {
        let kind = match timeout_kind {
            call::TxnTimeoutKind::Response => {
                crate::peer_failures::PeerFailureKind::ResponseTimeout
            }
            call::TxnTimeoutKind::Transaction => {
                crate::peer_failures::PeerFailureKind::TransactionTimeout
            }
        };
        ctx.metrics.record_peer_failure(
            dest,
            classify_b2bua_peer(ctx.outbound_proxy.as_deref(), dest),
            kind,
        );
    }

    // A self-release notice for a call this node holds no takeover copy of
    // has nothing to shed.
    if let Some(call_ref) = super::quiesced::notice_of(&event) {
        if !ctx.state.is_takeover(call_ref) {
            return;
        }
    }

    // Out-of-dialog OPTIONS keepalive: self-report readiness (ADR-0011 X6) on
    // its two axes (ADR-0050 §2), read by the front proxy's health probe, and
    // take the proxy's dead verdicts it carries (ADR-0050 §3).
    if let CallEvent::Sip { message, src, .. } = &event {
        if let SipMessage::Request(req) = message.as_ref() {
            if req.method() == "OPTIONS" && req.to().tag().is_none() {
                let resp = build_options_health_response(
                    &ctx.readiness,
                    &ctx.overload,
                    &ctx.options_tagger,
                    req,
                    &ctx.config.node_capabilities,
                );
                if let Ok(resp) = resp {
                    let _ = ctx.txn.send_response(resp, super::reply_to::reply_to(req, *src)).await;
                }
                super::dead_verdict::on_options(ctx, req);
                return;
            }
        }
    }

    let mut res = resolve(ctx, &event);
    let mut lookup = Lookup::Answered;
    // An initial INVITE copying a transaction of a decided call this node
    // holds the record of is that call's: the call is taken over and the copy
    // absorbed by the transaction the record rebuilds, which for the caller's
    // unanswered INVITE replaces the one the layer opened for the copy. One
    // placed here as another primary's backup and copying none belongs to
    // that primary and gets no final (its 100 Trying went out on receipt),
    // its transaction relinquished — never a second call.
    match initial_invite_copy(ctx, &event, &res).await {
        InitialInvite::Own => {}
        InitialInvite::CopyOf { hit, unanswered } => {
            if unanswered {
                relinquish_arrival(ctx, &event).await;
            }
            res.copy_of(hit, &event);
        }
        InitialInvite::Foreign => {
            ctx.metrics.bump_foreign_invite_copy_dropped();
            relinquish_arrival(ctx, &event).await;
            return;
        }
    }
    if res.call_ref.is_none() {
        // Acting-backup takeover BACKSTOP. The normal in-dialog key is the R-URI
        // `callref` param the B2BUA Contact stamps and the proxy preserves under
        // loose routing — so `resolve` (above) already keys the dialog from it,
        // and sip-txn `extract_ruri_call_ref` attributes the server txn by the
        // SAME key (the self-release count gate, ADR-0014). This branch only fires
        // when that param is absent AND our in-memory `sip_index` is empty — a
        // pure backup that never primary-served the call. Re-key the dialog from
        // the replica store's SIP index (the puller imported it) before declaring
        // the event unroutable, so a failed-over in-dialog request is not silently
        // dropped and the dialog can still terminate on the backup.
        match replica_takeover(ctx, &event).await {
            Ok(Some(hit)) => res.found_by_index(hit, &event),
            Ok(None) => {}
            Err(_) => lookup = Lookup::Failed,
        }
    }
    let call_ref = match res.call_ref.clone() {
        Some(r) => r,
        None => {
            super::unroutable::on_unroutable(ctx, &event, lookup).await;
            return;
        }
    };

    // ── Setup-CANCEL mark ──────────────────────────────────────────────
    // An out-of-dialog CANCEL means the txn layer just finalized the initial
    // INVITE (200 + 487 already on the wire). The `Cancelled` event queues on
    // the per-call FIFO BEHIND the initial-INVITE turn (waiting for a permit,
    // or parked on its decision round trip), so the call model cannot learn
    // the caller is gone until that turn ends — mark it here (the run loop) so
    // the turn (`process::initial_invite_turn`) skips the decision of a setup
    // not yet ruled, and drops a route/reject landing on the cancelled call.
    // The mark names the cancelled INVITE's CSeq: only that INVITE's turn
    // reads it. An in-dialog CANCEL targets one re-INVITE transaction, never
    // the call setup, and is not marked. Gated on that INVITE being here —
    // admitted and waiting for its turn, or the live call's own during its
    // decision round trip — so the setup window always marks, while the CANCEL
    // of an INVITE refused or discarded, or one reaching a queue whose call
    // was released, marks nothing no release would clear.
    if let CallEvent::Cancelled { in_dialog: false, invite_cseq: Some(cseq), .. } = &event {
        if admit::invite_here(ctx, &call_ref, *cseq) {
            ctx.state.mark_setup_cancelled(&call_ref, *cseq);
        }
    }

    let Some(class) = DispatchClass::of(&event, matched_client_txn) else { return };
    // A copy of a replicated call's INVITE opens no call: it queues as a
    // request of that call.
    let class = if class.is_new_call() && !res.initial_invite {
        DispatchClass::InDialogInvite
    } else {
        class
    };
    // A new INVITE is judged unless it is a copy of a call already here (live,
    // or admitted and not born yet, on the same CSeq). The check and the offer
    // run with no await between them.
    let mut unborn = None;
    if class.is_new_call() {
        if let CallEvent::Sip { message, src, .. } = &event {
            if let SipMessage::Request(req) = message.as_ref() {
                if !admit::is_copy(ctx, &call_ref, req) {
                    match admit::admit(ctx, &call_ref, req, *src, class).await {
                        Some(hold) => unborn = Some(hold),
                        None => return,
                    }
                }
            }
        }
    }
    let admitted = unborn.is_some();
    let turn = Turn { ctx: ctx.clone(), event, matched_client_txn, res, class, unborn };
    let offer = ctx.dispatcher.offer(&call_ref, turn, class);
    if admitted && matches!(offer.outcome, Outcome::Queued) {
        admit::queued(ctx);
    }
    if offer.crossed_lifetime_cap {
        ctx.reaper.on_lifetime_cap(&call_ref);
    }
    if offer.hit_overflow_ceiling {
        ctx.reaper.on_overflow_ceiling(&call_ref);
    }
    if let Outcome::Discarded(discarded) = offer.outcome {
        let Discarded { item, why, owed } = discarded;
        let admitted = item.unborn.is_some();
        OwedAnswer::of(ctx).render(&item.event, why, owed, admitted).await;
    }
}

/// Relinquish the server transaction the layer opened for the initial INVITE
/// `event` carries, sending nothing: another transaction answers it.
async fn relinquish_arrival(ctx: &RouterCtx, event: &CallEvent) {
    let CallEvent::Sip { message, .. } = event else { return };
    let SipMessage::Request(req) = message.as_ref() else { return };
    if let Some(key) = sip_txn::ServerTxnKey::of(req) {
        let _ = ctx.txn.relinquish_server(key).await;
    }
}

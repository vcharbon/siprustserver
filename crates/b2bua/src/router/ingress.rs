//! The pre-dispatch pipeline: every event enters here once. Data-path metrics,
//! per-peer timeout attribution, the acting-backup self-release notice, the
//! out-of-dialog OPTIONS health responder, resolution (an event resolving to
//! no call goes to [`super::unroutable`]), a new INVITE's router rungs of the
//! admission ladder ([`super::admit`]), then the offer of the event's
//! [`Turn`] to its call's queue under its dispatch class
//! ([`crate::dispatch::class`]). The router acts on the offer: an admitted
//! new INVITE spends its CPS token once its turn is queued, the reaper hears
//! a call past its lifetime cap or at its overflow ceiling, and a discarded
//! request is paid what it is owed ([`super::owed`]).

use std::sync::Arc;

use sip_message::SipMessage;

use super::admit;
use super::owed::OwedAnswer;
use super::peer_metrics::classify_b2bua_peer;
use super::release::{release_call, ReleaseKind};
use super::resolve::{replica_takeover, resolve};
use super::responses::build_options_health_response;
use super::turn::Turn;
use super::unroutable::Lookup;
use super::RouterCtx;
use crate::dispatch::{Discarded, DispatchClass, Outcome};
use b2bua_sdk::event::CallEvent;

pub(super) async fn on_event(ctx: &Arc<RouterCtx>, event: CallEvent) {
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
            sip_txn::TimeoutKind::Response => {
                crate::peer_failures::PeerFailureKind::ResponseTimeout
            }
            sip_txn::TimeoutKind::Transaction => {
                crate::peer_failures::PeerFailureKind::TransactionTimeout
            }
        };
        ctx.metrics.record_peer_failure(dest, classify_b2bua_peer(&ctx.config, dest), kind);
    }

    // ADR-0014 acting-backup self-release: the txn layer reports the last
    // transaction we served for a takeover copy has cleared; shed the live copy
    // (the `bak:` replica + reverse-flushed deltas remain). Re-checked under the
    // per-call lock, and the guard is held ACROSS the release — dropping it
    // between the re-check and `drop_local` re-opens the X11 double-serve
    // zombie window (ADR-0014): a parked handler could hydrate the still-
    // resident copy unmarked/unwatched and re-insert it after `drop_local`.
    // `release_call` takes no per-call lock itself, so holding the guard across
    // it is deadlock-free.
    if let CallEvent::CallQuiesced { call_ref } = &event {
        let call_ref = call_ref.clone();
        if ctx.state.is_takeover(&call_ref) {
            let _guard = ctx.state.lock(&call_ref).await;
            if ctx.state.is_takeover(&call_ref) {
                if ctx.txn.active_txn_count_for_call(&call_ref).await.unwrap_or(0) == 0 {
                    // Model Y (ADR-0020 X3): a takeover copy DEFERS its discharge
                    // to the live primary regardless of its state — it is never an
                    // independent CDR/limiter writer — so self-release
                    // unconditionally. An Active copy continues at the reclaiming
                    // primary (ADR-0014); a Terminating/Terminated copy was
                    // reverse-flushed in `process_result` and the primary
                    // discharges it exactly once (immediately if reconciling, on
                    // reboot via reclaim). A primary that never returns inside the
                    // replica TTL loses the CDR/limiter cleanup — the accepted
                    // double-failure.
                    if let Some(call) = ctx.state.peek(&call_ref) {
                        if matches!(
                            call.state,
                            call::CallModelState::Terminating | call::CallModelState::Terminated
                        ) {
                            // Belt-and-braces reverse-flush of the terminal state (a
                            // Terminated copy skips the process_result flush gate) so
                            // the primary's reconcile/reclaim has it — held with the
                            // normal replica TTL (`reboot_budget`).
                            ctx.state.flush(&call);
                        }
                    }
                    release_call(ctx, &call_ref, ReleaseKind::SelfRelease { ended: false }).await;
                } else {
                    // A fresh in-dialog request (a second takeover during a
                    // sustained partition) re-armed a transaction since this notice
                    // was emitted, and the txn layer's watch is one-shot — it was
                    // consumed delivering THIS CallQuiesced. Re-arm it so the
                    // eventual last-txn clear notifies us again; otherwise the
                    // takeover copy is stranded double-serving until its
                    // GlobalDuration backstop (hydrate never re-arms the watch for
                    // an already-resident copy: it returns fresh == false).
                    let _ = ctx.txn.watch_self_release(&call_ref).await;
                }
            }
        }
        return;
    }

    // Out-of-dialog OPTIONS keepalive: self-report readiness (ADR-0011 X6).
    // The front proxy probe keys on the status + Reason header text
    // (`sip-proxy::health::probe::classify_503`).
    if let CallEvent::Sip { message, src, .. } = &event {
        if let SipMessage::Request(req) = message.as_ref() {
            if req.method() == "OPTIONS" && req.to().tag().is_none() {
                let resp = build_options_health_response(
                    &ctx.readiness,
                    &ctx.overload,
                    &ctx.id_gen,
                    req,
                    &ctx.config.node_capabilities,
                );
                let _ = ctx.txn.send_response(resp, *src).await;
                return;
            }
        }
    }

    let mut res = resolve(ctx, &event);
    let mut lookup = Lookup::Answered;
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

    // `CallQuiesced`, the one event with no dispatch class, returned above.
    let Some(class) = DispatchClass::of(&event) else { return };
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
    let turn = Turn { ctx: ctx.clone(), event, res, class, unborn };
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

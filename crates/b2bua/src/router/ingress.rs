//! The pre-dispatch pipeline: every event enters here once. Data-path metrics,
//! per-peer timeout attribution, the acting-backup self-release notice, the
//! out-of-dialog OPTIONS health responder, resolution, the full-guarantee cap
//! shed, and finally the per-call FIFO dispatch.

use std::sync::Arc;

use sip_message::SipMessage;

use super::peer_metrics::classify_b2bua_peer;
use super::process::process;
use super::release::{release_call, ReleaseKind};
use super::resolve::{replica_takeover_call_ref, resolve};
use super::responses::{build_options_health_response, build_stateless_overload_503};
use super::RouterCtx;
use crate::event::CallEvent;

pub(super) async fn on_event(ctx: &Arc<RouterCtx>, event: CallEvent) {
    // Per-method / per-(method,code) data-path counters. Every inbound SIP
    // message lands here once, so this is the single chokepoint to meter them.
    if let CallEvent::Sip { message, .. } = &event {
        match message.as_ref() {
            SipMessage::Request(req) => ctx.metrics.record_request(req.method.as_str()),
            SipMessage::Response(resp) => ctx.metrics.record_response(resp.cseq.method.as_str(), resp.status),
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
            sip_txn::TimeoutKind::Response => crate::peer_failures::PeerFailureKind::ResponseTimeout,
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
                        if matches!(call.state, call::CallModelState::Terminating | call::CallModelState::Terminated) {
                            // Belt-and-braces reverse-flush of the terminal state (a
                            // Terminated copy skips the process_result flush gate) so
                            // the primary's reconcile/reclaim has it — held with the
                            // normal replica TTL (`reboot_budget`).
                            ctx.state.flush(&call);
                        }
                    }
                    release_call(ctx, &call_ref, ReleaseKind::SelfRelease).await;
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

    // Out-of-dialog OPTIONS keepalive: self-report readiness (S7, ADR-0011 X6).
    // The front proxy probe keys on the status + Reason header text
    // (`sip-proxy::health::probe::classify_503`).
    if let CallEvent::Sip { message, src } = &event {
        if let SipMessage::Request(req) = message.as_ref() {
            if req.method == "OPTIONS" && req.to.tag.is_none() {
                let resp =
                    build_options_health_response(&ctx.readiness, &ctx.overload, &ctx.id_gen, req);
                let _ = ctx.txn.send_response(resp, *src).await;
                return;
            }
        }
    }

    let mut res = resolve(ctx, &event);
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
        res.call_ref = replica_takeover_call_ref(ctx, &event).await;
    }
    let call_ref = match res.call_ref.clone() {
        Some(r) => r,
        None => {
            ctx.metrics.bump_unroutable_dropped();
            return;
        }
    };

    // ── Full-guarantee cap shed (ADR-0022) ────────────────────────────────────
    // At the per-call global cap, `dispatch` would SILENTLY drop a brand-new
    // call_ref's body before any call/txn context exists — leaving a caller who
    // already heard sip-txn's auto-100 on "100-then-silence" (the one full-queue
    // path neither the decision deadline nor the terminated-unanswered synthesis
    // can reach, because no call is ever born). Shed a NEW initial INVITE here
    // with a stateless 503 instead (mirrors the Tier-3 admission gate: stateless,
    // no per-call resources, sent through the INVITE server txn that carries the
    // 100). In-dialog events for an at-cap new call_ref stay on the silent
    // `dispatch` cap-drop (an in-dialog request with no live call is an orphan the
    // protocol resends / the peer 481s; only the initial INVITE owes a final).
    if res.initial_invite && ctx.dispatcher.would_drop_new_at_cap(&call_ref) {
        if let CallEvent::Sip { message, src } = &event {
            if let SipMessage::Request(req) = message.as_ref() {
                let resp = build_stateless_overload_503(
                    &ctx.id_gen,
                    req,
                    ctx.config.retry_after_base_sec,
                );
                let _ = ctx.txn.send_response(resp, *src).await;
            }
        }
        // Count it on the same cap counter (the cap WAS reached); the caller now
        // gets a 503 rather than silence.
        ctx.metrics.bump_cap_drop();
        return;
    }

    let ctx2 = ctx.clone();
    ctx.dispatcher.dispatch(
        &call_ref,
        Box::pin(async move {
            process(&ctx2, event, res).await;
        }),
    );
}

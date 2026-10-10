//! The live-path store-fault probes of an in-dialog event (ADR-0023): a
//! request fails closed, a keepalive audit fails open.

use std::net::SocketAddr;
use std::sync::Arc;

use call::helpers::cap_keepalive_fire_at;
use call::{Call, TimerEntry, TimerType};
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{Method, SipMessage};

use super::RouterCtx;
use crate::store::StoreFaultPoint;
use b2bua_sdk::event::CallEvent;

/// Live-path store-fault probes for in-dialog events (ADR-0023), with the
/// defined degraded-mode semantics. Returns `true` when the event was consumed
/// by a fault path:
///   - in-dialog SIP *request* (BYE, re-INVITE, …): fail CLOSED — 500 through
///     the server txn, call + state untouched (deliberately distinct from the
///     481 lookup-MISS: the call may well exist, the store just cannot say). A
///     retry after recovery proceeds normally. ACK is never answered (RFC 3261
///     §17) — dropped, as the orphan path drops it.
///   - Keepalive audit timer: fail OPEN — skip this probe cycle but RE-ARM the
///     timer so liveness detection resumes next interval; a store fault alone
///     must never tear down an established call (protected-calls invariant,
///     docs/testing/ha-acceptance.md). A fire of a call no longer resident
///     passes on, the router dropping it; one the resident copy no longer
///     holds at its generation is stale and dropped, the live one left armed.
///   - everything else (responses, CANCEL/timeout/internal events) is
///     deliberately un-probed: those paths owe no store-derived answer, and
///     absorbing e.g. a keepalive OPTIONS-200 here would convert a store fault
///     into a KeepaliveTimeout teardown of a healthy call.
pub(super) async fn in_dialog_store_fault_gate(
    ctx: &Arc<RouterCtx>,
    event: &CallEvent,
    call_ref: &str,
    now_ms: i64,
) -> bool {
    if let CallEvent::Sip { message, src, .. } = event {
        if let SipMessage::Request(req) = message.as_ref() {
            if ctx.store_faults.check(StoreFaultPoint::LiveInDialog).is_err() {
                let resident = ctx.state.peek(call_ref);
                refuse_on_store_fault(ctx, req, *src, resident.as_ref()).await;
                return true;
            }
        }
    }
    if let CallEvent::Timer { key, generation, .. } = event {
        if key.timer_type != TimerType::Keepalive {
            return false;
        }
        if ctx.store_faults.check(StoreFaultPoint::LiveAudit).is_err() {
            // A fire of a call no longer here is the router's to drop with the
            // call's ephemera, never re-armed; one its resident copy no longer
            // holds is stale: re-arming it would displace the live instance.
            let Some(resident) = ctx.state.peek(call_ref) else {
                return false;
            };
            if !call::helpers::holds_timer(&resident, key, *generation) {
                tracing::debug!(%call_ref, timer = ?key, generation, "stale keepalive fire dropped");
                return true;
            }
            ctx.metrics.bump_store_fault_audit_skipped();
            // Re-arm at the config cadence — the same interval the `keepalive`
            // rule re-arms with, read from config because the call body is what
            // we could not fetch, and held to the same ledger ceiling. Runtime
            // driver only: the serialized `call.timers` intent stays untouched
            // (the call is untouched) and still holds the fired instance, so the
            // re-arm keeps its generation and its fire reaches the call; a later
            // HA restore sanitizes past-due entries.
            let interval_ms = ctx.config.keepalive_interval_sec * 1000;
            let entry = TimerEntry {
                key: key.clone(),
                generation: *generation,
                fire_at: cap_keepalive_fire_at(now_ms + interval_ms, now_ms, interval_ms),
            };
            let incarnation = event.incarnation().map(str::to_string);
            ctx.timers.schedule(entry, call_ref.to_string(), incarnation).await;
            return true;
        }
    }
    false
}

/// Fail an in-dialog request CLOSED on a store fault (ADR-0023): 500 through
/// its server transaction, an ACK dropped unanswered (RFC 3261 §17). Never the
/// 481 of a lookup miss: the dialog may exist, the store cannot say. A call
/// still resident (`resident`) states its headers on the refusal.
pub(super) async fn refuse_on_store_fault(
    ctx: &RouterCtx,
    req: &sip_message::SipRequest,
    src: SocketAddr,
    resident: Option<&Call>,
) {
    if req.method() != Method::Ack {
        let mut resp =
            generate_response(req, 500, "Server Internal Error", &GenerateResponseOpts::default());
        if let Some(call) = resident {
            crate::rules::stated_headers::stamp_answer(call, req, &mut resp);
        }
        let _ = ctx.txn.send_response(resp, src).await;
    }
    ctx.metrics.bump_store_fault_rejected();
}

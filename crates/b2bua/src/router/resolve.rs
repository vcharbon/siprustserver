//! Event → call resolution: derive the `callRef`, source leg and direction for
//! every event kind (synchronously), including the acting-backup takeover
//! re-key from the replica store's SIP index.

use call::Direction;
use sip_message::message_helpers::parse_uri_params;
use sip_message::SipMessage;

use super::RouterCtx;
use crate::event::CallEvent;

/// How an event resolves to a call + the leg it arrived on.
pub(super) struct Resolution {
    pub(super) call_ref: Option<String>,
    pub(super) source_leg_id: String,
    pub(super) direction: Direction,
    pub(super) initial_invite: bool,
}

/// Resolve the `callRef` + source leg for an event (synchronous, no blocking).
pub(super) fn resolve(ctx: &RouterCtx, event: &CallEvent) -> Resolution {
    match event {
        CallEvent::Sip { message, .. } => match message.as_ref() {
            SipMessage::Request(req) => {
                if req.method == "INVITE" && req.to.tag.is_none() {
                    let call_ref = call::derive_call_ref(
                        &ctx.config.self_ordinal,
                        &req.call_id,
                        req.from.tag.as_deref().unwrap_or(""),
                    );
                    return Resolution {
                        call_ref: Some(call_ref),
                        source_leg_id: "a".into(),
                        direction: Direction::FromA,
                        initial_invite: true,
                    };
                }
                // In-dialog request: read our cr/lg from the Request-URI params.
                // NB `parse_uri_params` lower-cases param NAMES (URI params are
                // case-insensitive per RFC 3261 §19.1.1), so the stamped `callRef`
                // is keyed as `callref`. The primary path masks a mismatch via the
                // in-memory `sip_index` fallback below; the acting-backup takeover
                // path has no such index, so the param IS the only key — read it by
                // its normalised (lower-case) name.
                let params = parse_uri_params(&req.uri);
                let leg = params
                    .get("leg")
                    .map(|v| crate::stack_identity::decode_param(v))
                    .unwrap_or_else(|| "a".into());
                let call_ref = params
                    .get("callref")
                    .map(|v| crate::stack_identity::decode_param(v))
                    .or_else(|| {
                        ctx.state.resolve_from_sip_key_sync(
                            &req.call_id,
                            req.from.tag.as_deref().unwrap_or(""),
                        )
                    });
                Resolution {
                    direction: leg_direction(&leg),
                    call_ref,
                    source_leg_id: leg,
                    initial_invite: false,
                }
            }
            SipMessage::Response(resp) => {
                // Response: read our cr/lg from the top Via we stamped.
                let ids = via_cr_lg(resp.headers.first().map(|h| h.value.as_str()))
                    .or_else(|| {
                        resp.headers
                            .iter()
                            .find(|h| h.name.eq_ignore_ascii_case("via"))
                            .and_then(|h| via_cr_lg(Some(&h.value)))
                    })
                    .unwrap_or(ViaIds { cr: None, lg: "a".into() });
                let call_ref = ids.cr.or_else(|| {
                    ctx.state.resolve_from_sip_key_sync(&resp.call_id, resp.to.tag.as_deref().unwrap_or(""))
                });
                Resolution {
                    direction: leg_direction(&ids.lg),
                    call_ref,
                    source_leg_id: ids.lg,
                    initial_invite: false,
                }
            }
        },
        CallEvent::Cancelled { call_id, from_tag, .. } => {
            // A CANCEL races the very INVITE it cancels. The initial-INVITE body
            // `create()`s (and indexes) the call on the per-call FIFO *worker*,
            // asynchronously — whereas this `resolve` runs in the run loop the
            // instant the txn layer emits `Cancelled`. So the `sip_index` may not
            // be populated yet, and a sync index miss would drop the CANCEL as
            // unroutable, leaking the b-leg the (still-parked) decision is about
            // to build. DERIVE the callRef the same way the INVITE did
            // (`derive_call_ref(self, callId, fromTag)`) when the index misses, so
            // the CANCEL resolves to the SAME call regardless of create() timing;
            // FIFO ordering then guarantees `handle-cancel` runs after the INVITE
            // body has built the call + b-leg. Deriving with `self_ordinal` is
            // correct here because a CANCEL only targets a brand-new INVITE this
            // node is primary-serving (build_initial_call used the same ordinal);
            // ACK/BYE cannot hit this path — they require an established dialog, so
            // the call (and its index) already exist. A genuinely orphan CANCEL
            // (no INVITE ever) resolves to a callRef with no live call and is
            // reaped cleanly via the orphan path in `process`.
            let call_ref = ctx
                .state
                .resolve_from_sip_key_sync(call_id, from_tag)
                .unwrap_or_else(|| {
                    call::derive_call_ref(&ctx.config.self_ordinal, call_id, from_tag)
                });
            Resolution {
                call_ref: Some(call_ref),
                source_leg_id: "a".into(),
                direction: Direction::FromA,
                initial_invite: false,
            }
        }
        CallEvent::Timeout { call_ref, leg_id, .. } => {
            let leg = leg_id.clone().unwrap_or_else(|| "a".into());
            Resolution {
                direction: leg_direction(&leg),
                call_ref: call_ref.clone(),
                source_leg_id: leg,
                initial_invite: false,
            }
        }
        CallEvent::Timer { call_ref, leg_id, .. } => {
            let leg = leg_id.clone().unwrap_or_else(|| "a".into());
            Resolution {
                direction: leg_direction(&leg),
                call_ref: Some(call_ref.clone()),
                source_leg_id: leg,
                initial_invite: false,
            }
        }
        CallEvent::InternalEvent { call_ref, .. } => Resolution {
            call_ref: Some(call_ref.clone()),
            source_leg_id: "a".into(),
            direction: Direction::FromA,
            initial_invite: false,
        },
        // Handled (and returned) in `on_event` before `resolve` is ever called.
        CallEvent::CallQuiesced { .. } => unreachable!("CallQuiesced is handled before resolve"),
    }
}

/// Recover the takeover `callRef` for an in-dialog SIP request from the replica
/// store's SIP index (the acting-backup production path). Only in-dialog requests
/// (those carrying a To-tag) are candidates; an initial request, a response, or a
/// non-SIP event is never a dialog takeover. `None` when not applicable or no
/// replica matches — the caller then treats the event as unroutable.
pub(super) async fn replica_takeover_call_ref(ctx: &RouterCtx, event: &CallEvent) -> Option<String> {
    let CallEvent::Sip { message, .. } = event else { return None };
    let SipMessage::Request(req) = message.as_ref() else { return None };
    if req.to.tag.is_none() {
        return None; // initial request — a brand-new dialog, not a takeover
    }
    ctx.state
        .resolve_from_replica_index(&req.call_id, req.from.tag.as_deref().unwrap_or(""))
        .await
}

fn leg_direction(leg: &str) -> Direction {
    if leg == "a" {
        Direction::FromA
    } else {
        Direction::FromB
    }
}

/// The `;cr=`/`;lg=` identity params stamped on a Via we emitted.
struct ViaIds {
    cr: Option<String>,
    lg: String,
}

/// Extract the [`ViaIds`] from a Via header value's `;cr=`/`;lg=` params.
fn via_cr_lg(via: Option<&str>) -> Option<ViaIds> {
    let via = via?;
    if !via.contains("cr=") && !via.contains("lg=") {
        return None;
    }
    let mut cr = None;
    let mut lg = "a".to_string();
    for part in via.split(';').skip(1) {
        let (k, v) = part.split_once('=').unwrap_or((part.trim(), ""));
        match k.trim() {
            "cr" => cr = Some(crate::stack_identity::decode_param(v.trim())),
            "lg" => lg = crate::stack_identity::decode_param(v.trim()),
            _ => {}
        }
    }
    Some(ViaIds { cr, lg })
}

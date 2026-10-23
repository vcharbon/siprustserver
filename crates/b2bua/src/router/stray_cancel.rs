//! The router's answer to a CANCEL no INVITE transaction took (RFC 3261 §9.2).

use b2bua_sdk::event::CallEvent;
use call::Call;
use sip_message::{Method, SipMessage, SipResponse};

use super::responses::build_481;
use super::RouterCtx;

/// RFC 3261 §9.2 for a CANCEL that matched no INVITE transaction in the layer
/// and none a call here could rebuild: 481 under `to_tag`, the tag the call's
/// final to the INVITE carried where a call resolves, and no effect on any
/// call. The 481 sent when `event` was such a CANCEL.
pub(super) async fn reject_stray_cancel(
    ctx: &RouterCtx,
    call: &Call,
    leg_id: &str,
    to_tag: Option<&str>,
    event: &CallEvent,
) -> Option<SipResponse> {
    let CallEvent::Sip { message, src, .. } = event else { return None };
    let SipMessage::Request(req) = message.as_ref() else { return None };
    if req.method() != Method::Cancel {
        return None;
    }
    // A message of the leg the CANCEL resolved to: it takes the call's stated
    // headers.
    let mut refusal = build_481(req, to_tag).ok()?;
    for skipped in crate::rules::stated_headers::stamp_response(call, leg_id, &mut refusal) {
        let detail = format!("stated header: {skipped}");
        crate::model_invariants::stated_text_skipped(&ctx.metrics, call, &detail);
    }
    let _ = ctx.txn.send_response(refusal.clone(), *src).await;
    Some(refusal)
}

//! An outbound emission whose next hop is not an IP literal. The worker
//! resolves no name (a name reaches the wire through the outbound proxy), so
//! the message cannot leave. A request that would open a client transaction
//! draws no response, so the call gets at once the response `Timeout` that
//! transaction reports for a dead hop, instead of waiting on a B2BUA timer.
//! Every other emission (a response, an ACK, a CANCEL, a retained datagram)
//! has no transaction to fail and is counted.

use b2bua_sdk::event::CallEvent;
use call::TxnTimeoutKind;

use super::RouterCtx;
use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode};

/// Account for `eff` of call `incarnation`, which names no IP-literal next
/// hop.
pub(super) fn fail_unaddressable(
    ctx: &RouterCtx,
    call_ref: &str,
    incarnation: &str,
    eff: &OutboundSipEffect,
) {
    let kind = match &eff.body {
        OutboundBody::Request(_) => "request",
        OutboundBody::Response(_) => "response",
        OutboundBody::Datagram(_) => "datagram",
    };
    ctx.metrics.record_outbound_unaddressable(kind);
    tracing::debug!(
        %call_ref,
        label = %eff.label,
        host = %eff.destination.0,
        port = eff.destination.1,
        "outbound message not sent: its next hop is not an IP literal"
    );
    let (OutboundBody::Request(req), OutboundTxnMode::NewClient(_)) = (&eff.body, &eff.mode) else {
        return;
    };
    let Some(branch) = req.top_via().branch() else {
        return;
    };
    let failed = CallEvent::Timeout {
        branch: branch.to_string(),
        call_ref: Some(call_ref.to_string()),
        leg_id: eff.leg_id.clone(),
        method: Some(req.method().as_str().to_string()),
        cseq: req.cseq().seq(),
        destination: None,
        timeout_kind: TxnTimeoutKind::Response,
        incarnation: None,
    };
    let _ = ctx.reentry_tx.send(failed.stamped(incarnation));
}

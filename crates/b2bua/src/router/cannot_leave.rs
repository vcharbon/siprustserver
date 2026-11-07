//! An outbound emission that cannot leave: its next hop is not an IP literal
//! (the worker resolves no name; a name reaches the wire through the outbound
//! proxy), or it is a request opening a client transaction whose datagram is
//! past the largest UDP payload of its destination's family (ADR-0001). A
//! request that would open a client transaction draws no response, so the
//! call gets at once the response `Timeout` that transaction reports for a
//! dead hop, instead of waiting on a timer. Every other emission with no IP
//! literal (a response, an ACK, a CANCEL, a retained datagram) has no
//! transaction to fail and is counted. Each one is recorded into the unsent
//! wave of its next hop ([`crate::lifecycle::unsent_waves`]), which logs an
//! episode at warn with its reason and its latest message's Request-URI.

use std::net::SocketAddr;

use b2bua_sdk::event::CallEvent;
use call::TxnTimeoutKind;

use super::RouterCtx;
use crate::effects::{OutboundBody, OutboundSipEffect, OutboundTxnMode};
use crate::lifecycle::Unsent;

/// Why an emission cannot leave, the `reason` label of
/// `b2bua_outbound_unsent_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CannotLeave {
    /// Its next hop is not an IP literal.
    Unaddressable,
    /// It is a new client request past the largest UDP payload.
    TooLong,
}

impl CannotLeave {
    pub(super) fn label(self) -> &'static str {
        match self {
            CannotLeave::Unaddressable => "unaddressable",
            CannotLeave::TooLong => "too_long",
        }
    }
}

/// Whether `eff`, bound for `dest`, is a request opening a client transaction
/// whose datagram is past the largest UDP payload of `dest`'s family. Its image
/// is the datagram the transaction layer sends.
pub(super) fn too_long(eff: &OutboundSipEffect, dest: SocketAddr) -> bool {
    match (&eff.body, &eff.mode) {
        (OutboundBody::Request(req), OutboundTxnMode::NewClient(_)) => {
            req.image().len() > sip_net::max_udp_payload(dest)
        }
        _ => false,
    }
}

/// Account for `eff` of call `incarnation`, which cannot leave for `why`.
pub(super) fn fail(
    ctx: &RouterCtx,
    call_ref: &str,
    incarnation: &str,
    eff: &OutboundSipEffect,
    why: CannotLeave,
) {
    let kind = match &eff.body {
        OutboundBody::Request(_) => "request",
        OutboundBody::Response(_) => "response",
        OutboundBody::Datagram(_) => "datagram",
    };
    ctx.metrics.record_outbound_unsent(why.label(), kind);
    let request_uri = match &eff.body {
        OutboundBody::Request(req) => Some(req.request_uri().to_string()),
        OutboundBody::Response(_) | OutboundBody::Datagram(_) => None,
    };
    let hop = format!("{}:{}", eff.destination.0, eff.destination.1);
    let unsent = Unsent {
        call_ref: call_ref.to_string(),
        label: eff.label.clone(),
        reason: why.label(),
        request_uri,
    };
    ctx.unsent_waves.record_with(&hop, kind, 1, unsent);
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

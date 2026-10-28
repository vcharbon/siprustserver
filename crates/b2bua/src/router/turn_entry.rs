//! A turn's outbox entry (ADR-0055 §3): everything the turn sends off the
//! node, run in one go when the call's outbox releases it. It runs the turn's
//! first provisional of a re-INVITE, then the transactions it relinquishes, its
//! SIP in emission order, its limiter releases and its requests to outside
//! services, and marks the turn [`Left`](TurnPoint::Left) last.

use std::sync::Arc;

use call::{Call, TxnKind};
use sip_message::header::Via;
use sip_message::{Method, SipRequest};

use super::callouts;
use super::interpret::{reach, TurnKind};
use super::RouterCtx;
use crate::effects::{FireAndForgetEffect, OutboundBody, OutboundSipEffect, OutboundTxnMode};
use crate::turn_probe::{Released, TurnPoint};

/// What one turn sends off the node.
pub(super) struct TurnEntry {
    pub ctx: Arc<RouterCtx>,
    pub call_ref: String,
    /// The record the turn landed, shared with the store: what a traced
    /// send and a callout read.
    pub call: Arc<Call>,
    pub now_ms: i64,
    /// The re-INVITE whose first provisional the turn's write records.
    pub trying: Option<SipRequest>,
    /// The Vias of the answered INVITEs whose server transactions go.
    pub relinquish: Vec<Via>,
    pub outbound: Vec<OutboundSipEffect>,
    /// The limiter holds the turn releases.
    pub limiter_releases: Vec<String>,
    /// The turn's requests to outside services.
    pub callouts: Vec<FireAndForgetEffect>,
    /// The turn's `Left` mark: its `(primary, backup)` version at `Logged`,
    /// whether it wrote, and what it was taken on.
    pub mark: ((i64, i64), bool, TurnKind),
}

impl TurnEntry {
    /// An entry of `call`'s turn sending `outbound` and nothing else, marked
    /// as a turn that wrote nothing.
    pub fn sending(
        ctx: &Arc<RouterCtx>,
        call_ref: &str,
        call: Arc<Call>,
        now_ms: i64,
        outbound: Vec<OutboundSipEffect>,
    ) -> Self {
        Self {
            ctx: ctx.clone(),
            call_ref: call_ref.to_string(),
            call,
            now_ms,
            trying: None,
            relinquish: Vec::new(),
            outbound,
            limiter_releases: Vec::new(),
            callouts: Vec::new(),
            mark: ((0, 0), false, TurnKind::default()),
        }
    }

    /// Send everything, then mark the turn left, `released` as it was.
    pub async fn run(self, released: Released) {
        let ctx = &self.ctx;
        let call_ref = self.call_ref.as_str();
        if let Some(reinvite) = &self.trying {
            let _ = ctx.txn.release_trying(reinvite).await;
        }
        for via in &self.relinquish {
            if let Some(branch) = via.branch() {
                let key = sip_txn::ServerTxnKey::invite(
                    branch.to_string(),
                    sip_message::header::SentBy::of(via),
                );
                let _ = ctx.txn.relinquish_server(key).await;
            }
        }
        for eff in &self.outbound {
            emit(ctx, call_ref, &self.call, eff, self.now_ms).await;
        }
        for key in &self.limiter_releases {
            ctx.limiter.release(key);
            if crate::trace::sampled(&self.call) {
                crate::trace::emit::limiter(&self.call, self.now_ms, "release queued", key);
            }
        }
        for eff in self.callouts {
            call_out(ctx, &self.call, eff);
        }
        let (version, wrote, kind) = self.mark;
        reach(ctx, call_ref, TurnPoint::Left, version, wrote, kind, Some(released)).await;
    }
}

/// Start the off-turn request `eff` of `call` to an outside service, whose
/// answer a later turn takes (see `callouts`).
fn call_out(ctx: &Arc<RouterCtx>, call: &Call, eff: FireAndForgetEffect) {
    match eff {
        FireAndForgetEffect::ReferAsyncHttp { call_ref, request } => {
            callouts::spawn_refer_callout(ctx, call, call_ref, request);
        }
        FireAndForgetEffect::ServiceHttpRequest {
            call_ref,
            correlation_id,
            endpoint,
            method,
            headers,
            body,
            content_type,
            timeout_ms,
        } => {
            callouts::spawn_service_http_callout(
                ctx,
                callouts::ServiceHttpCallout {
                    to: callouts::Caller::of(call_ref, call),
                    correlation_id,
                    endpoint,
                    method,
                    headers,
                    body,
                    content_type,
                    timeout_ms,
                },
            );
        }
        FireAndForgetEffect::FailureAsyncHttp { call_ref, request, limiter, deadline } => {
            callouts::spawn_failure_callout(ctx, call, call_ref, request, limiter, deadline);
        }
        FireAndForgetEffect::ReleaseAsyncHttp { call_ref, request, limiter, deadline } => {
            callouts::spawn_release_callout(ctx, call, call_ref, request, limiter, deadline);
        }
        FireAndForgetEffect::LimiterAdmit {
            call_ref,
            correlation_id,
            key,
            change,
            held,
            entries,
        } => {
            callouts::spawn_limiter_admit_callout(
                ctx,
                callouts::LimiterAdmitCallout {
                    call_ref,
                    correlation_id,
                    key,
                    change,
                    held,
                    entries,
                },
            );
        }
        // Taken in the turn: it never leaves the node.
        FireAndForgetEffect::Reenter(_) => {}
    }
}

/// Put the outbound SIP `eff` of `call` on the wire.
async fn emit(ctx: &RouterCtx, call_ref: &str, call: &Call, eff: &OutboundSipEffect, now_ms: i64) {
    let Some(dest) = sip_net::ip_socket_addr(&eff.destination.0, eff.destination.1) else {
        super::unaddressable::fail_unaddressable(ctx, call_ref, call.incarnation(), eff);
        return;
    };
    // Meter outbound requests we originate/relay (the in-dialog keepalive
    // OPTIONS lands here) — pairs with inbound responses_total{OPTIONS,200} to
    // isolate the keepalive round-trip (sent vs answered) on the b2bua itself.
    if let OutboundBody::Request(req) = &eff.body {
        ctx.metrics.record_request_out(req.method().as_str());
    }
    // Traced call: the message as it leaves, raw (ADR-0026). A typed message
    // is recorded as its image — the datagram the transaction layer sends —
    // and a retained datagram as the bytes it is, so a recorded rung is
    // what left the socket, never a re-render of it.
    if crate::trace::sampled(call) {
        let wire: &[u8] = match &eff.body {
            OutboundBody::Request(req) => req.image(),
            OutboundBody::Response(resp) => resp.image(),
            OutboundBody::Datagram(emission) => emission.wire().0,
        };
        crate::trace::emit::sip_out(call, now_ms, dest, wire);
    }
    match (&eff.body, &eff.mode) {
        // A retained emission's repeat (RFC 3261 §13.3.1.4 / §13.2.2.4,
        // RFC 3262 §3): the bytes as they are, past every transaction — the
        // server txn that sent the original is `Accepted` and would drop a
        // second final; the ACK never had one. Counted as it leaves, under
        // the label captured when it was retained.
        (OutboundBody::Datagram(emission), _) => {
            let repeated = emission.repeated();
            ctx.metrics.record_retransmit(
                emission.repeat().ladder(),
                repeated.method(),
                repeated.code(),
            );
            let _ = ctx.txn.send_raw(emission.wire().0.to_vec(), dest).await;
        }
        // A response goes through its server transaction, whatever the mode
        // says: `Raw` is for requests, and the only raw path for response
        // bytes is a retained `Datagram` (ADR-0032 X3). The layer sends the
        // response's image verbatim, so what a rule retained from that
        // same image is what leaves here.
        (OutboundBody::Response(resp), mode) => {
            debug_assert!(
                !matches!(mode, OutboundTxnMode::Raw),
                "a raw response bypass is a retained datagram, never a re-serialized Response: {}",
                eff.label
            );
            // The layer holds the To-tag to the bound one; a rule that
            // retained this image for a repeat must have composed it
            // right, or the repeat and the wire disagree.
            if let Ok(sent) = ctx.txn.send_response(resp.clone(), dest).await {
                debug_assert!(
                    sent == *resp.image(),
                    "{}: the response left re-rendered under the bound To-tag",
                    eff.label
                );
            }
        }
        (OutboundBody::Request(req), OutboundTxnMode::NewClient(kind)) => {
            let _ = ctx.txn.send_request(req.clone(), dest, *kind).await;
        }
        (OutboundBody::Request(req), OutboundTxnMode::Raw) => {
            // A CANCEL reuses its INVITE's branch, and the INVITE client txn
            // owns WHEN it may go on the wire (RFC 3261 §9.1: held until the
            // branch's first provisional, dropped at Timer B) — so it goes
            // through `send_request`, whose CANCEL path sends raw once the
            // txn allows it. An ACK goes through it too, raw: the Accepted
            // INVITE client txn of its 2xx keeps it, and replays it to a
            // repeat of that 2xx once this call is released (RFC 6026 §7.2).
            if matches!(*req.method(), Method::Ack | Method::Cancel) {
                let _ = ctx.txn.send_request(req.clone(), dest, TxnKind::Invite).await;
            } else {
                let _ = ctx.txn.send_raw(req.image().to_vec(), dest).await;
            }
        }
        (OutboundBody::Request(req), OutboundTxnMode::ServerResponse) => {
            // A request tagged ServerResponse is a misuse; send raw as a fallback.
            let _ = ctx.txn.send_raw(req.image().to_vec(), dest).await;
        }
    }
}

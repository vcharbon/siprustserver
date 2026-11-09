//! Self-generated UAS finals: the synthesized response to its stamped top Via's
//! response target (RFC 3261 §18.2.2, RFC 3581 §4), the refusal of a new request
//! (one 503 for every copy, [`crate::refusals`]), the select-failure → 503
//! mapping, and the rule that an ACK is never answered (RFC 3261 §17): a reject
//! of an ACK drops it. The ACK to a non-2xx final sent here is recognised by its
//! To-tag (`route`), so sending one writes no memo.
//! Relayed responses do NOT live here — see `core/response`.

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::HeaderValue;
use sip_message::types::SipHeader;
use sip_message::{Method, SipRequest, SipResponse};

use crate::observability::metrics::{Direction, MessageResult, NoTargetDrop, RoutingDecisionKind};
use crate::strategy::SelectError;
use crate::trace::emit;
use sip_message::received::{observed_source, response_destination};

use super::super::ProxyCore;
use super::{Arrival, RouteOutcome};

/// A typed value on the response generator's extra-header seam, which still
/// takes wire lines.
pub(super) fn extra_header(value: impl HeaderValue) -> SipHeader {
    let name = value.name();
    SipHeader { name: name.as_wire_str().into(), value: value.to_wire().into() }
}

impl ProxyCore {
    /// Reject `req` locally: answer it with `status`, counted under `metric`.
    /// An ACK is never answered — nothing awaits a response to it — so a
    /// rejected ACK is dropped, counted `ack_not_answered`; an ACK no worker
    /// takes is a request dropped for want of a target (`no_target_dropped`).
    pub(super) async fn reply_reject(
        &self,
        req: &SipRequest,
        arrival: Arrival,
        status: u16,
        reason: &str,
        extra: &[SipHeader],
        metric: &str,
    ) -> RouteOutcome {
        if req.method() == Method::Ack {
            self.metrics.record_reject("ack_not_answered");
        } else {
            self.reply(req, arrival, status, reason, extra).await;
            self.metrics.record_reject(metric);
        }
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }

    /// A request whose forward the socket refused as past the largest UDP
    /// payload: 513 (RFC 3261 §21.5.14), counted `message_too_large` once,
    /// every later copy drawing the same 513 from
    /// [`repeat_refusal`](Self::repeat_refusal) and its CANCEL a 200. An ACK is
    /// dropped.
    pub(super) async fn reply_too_large(&self, req: &SipRequest, arrival: Arrival) -> RouteOutcome {
        if req.method() == Method::Ack {
            self.metrics.record_reject("ack_not_answered");
        } else {
            let refused = self.refusals.refuse_too_large(req, arrival.src, self.now_ms());
            if let Ok(resp) = refused {
                self.send_own_final(req, arrival, &resp).await;
            }
            self.metrics.record_reject("message_too_large");
        }
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }

    /// A Request-URI this hop cannot read: 400, counted `malformed_request_uri`.
    pub(super) async fn reply_malformed_uri(
        &self,
        req: &SipRequest,
        arrival: Arrival,
    ) -> RouteOutcome {
        self.reply_reject(req, arrival, 400, "Bad Request", &[], "malformed_request_uri").await
    }

    /// A Request-URI whose scheme names no host this hop can forward to: 416,
    /// counted `unsupported_uri_scheme`.
    pub(super) async fn reply_unsupported_scheme(
        &self,
        req: &SipRequest,
        arrival: Arrival,
    ) -> RouteOutcome {
        self.reply_reject(
            req,
            arrival,
            416,
            "Unsupported URI Scheme",
            &[],
            "unsupported_uri_scheme",
        )
        .await
    }

    /// Refuse the new request `req` with a 503, `retry_after_sec` being the
    /// refusing stage's hint, counted once under `metric`: every later copy
    /// draws the same 503 from [`repeat_refusal`](Self::repeat_refusal).
    pub(super) async fn refuse_new(
        &self,
        req: &SipRequest,
        arrival: Arrival,
        retry_after_sec: u32,
        metric: &str,
    ) -> RouteOutcome {
        // A request whose lines no response can echo is refused unanswered.
        if let Ok(resp) = self.refusals.refuse(req, arrival.src, retry_after_sec, self.now_ms()) {
            self.send_own_final(req, arrival, &resp).await;
        }
        self.metrics.record_reject(metric);
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }

    /// Answer a copy of a request refused within 64·T1 with that refusal's
    /// bytes, judged and counted no further. `None` when `req` is no such copy.
    pub(super) async fn repeat_refusal(
        &self,
        req: &SipRequest,
        arrival: Arrival,
    ) -> Option<RouteOutcome> {
        let resp = self.refusals.repeat(req, arrival.src, self.now_ms())?;
        self.send_own_final(req, arrival, &resp).await;
        Some(RouteOutcome { decision: RoutingDecisionKind::Reject, target: None })
    }

    /// Answer 200 to a CANCEL of an INVITE this hop refused within 64·T1: the
    /// proxy is that INVITE's UAS and its final is already sent (RFC 3261
    /// §9.2), so the CANCEL goes no further. `None` when `req` cancels no such
    /// INVITE.
    pub(super) async fn cancel_refused(
        &self,
        req: &SipRequest,
        arrival: Arrival,
    ) -> Option<RouteOutcome> {
        let resp = self.refusals.cancel(req, arrival.src, self.now_ms())?;
        self.send_own_final(req, arrival, &resp).await;
        Some(RouteOutcome { decision: RoutingDecisionKind::Cancel, target: None })
    }

    /// Synthesize a UAS response, the source recorded on its topmost Via (RFC
    /// 3261 §18.2.1, RFC 3581 §4).
    async fn reply(
        &self,
        req: &SipRequest,
        arrival: Arrival,
        status: u16,
        reason: &str,
        extra: &[SipHeader],
    ) {
        let opts = GenerateResponseOpts {
            to_tag: Some(self.refusals.to_tag(req)),
            extra_headers: extra.to_vec(),
            incoming_source: Some(observed_source(arrival.src)),
            ..Default::default()
        };
        if let Ok(resp) = generate_response(req, status, reason, &opts) {
            self.send_own_final(req, arrival, &resp).await;
        }
    }

    /// Send `resp`, the proxy's own final to `req`.
    async fn send_own_final(&self, req: &SipRequest, arrival: Arrival, resp: &SipResponse) {
        self.send_own_final_to(req.call_id().as_str(), arrival, resp).await;
    }

    /// Send `resp`, the proxy's own final to a request of call `call_id`, from
    /// the face that request arrived on (RFC 3581 §4) to the response target of
    /// its stamped top Via, the one address every response to that request
    /// goes to. The request's source where that target names no IP address.
    pub(super) async fn send_own_final_to(
        &self,
        call_id: &str,
        arrival: Arrival,
        resp: &SipResponse,
    ) {
        let dst = response_destination(resp.top_via()).unwrap_or(arrival.src);
        // A recipe freezes into its own image, and that image IS the wire form.
        self.send_response(resp.image(), dst, arrival.face).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Responded);
        // This is the proxy's ONLY self-generated-final seam, so it is the only
        // place a traced call's own 503 / 483 / 420 / 400 becomes a `sip.out`.
        // A shed call never reaches a downstream hop, so without this its span
        // would show the INVITE arriving and nothing leaving.
        emit::responded(&self.traces, call_id, self.trace_ms(), dst, resp.image());
    }

    /// Refuse the request a `select_for_new_dialog` failure leaves without a
    /// target. Each failure carries its own `Retry-After` base and reject
    /// label; the 503 itself names neither (no `Reason`). An ACK is dropped,
    /// and so is any request with no target before the proxy's first worker
    /// view: its sender's retransmission meets the view (ADR-0050 §5).
    pub(super) async fn reply_select_failure(
        &self,
        req: &SipRequest,
        arrival: Arrival,
        err: SelectError,
    ) -> RouteOutcome {
        let (retry_after, metric) = match &err {
            SelectError::NoTarget { .. } => (5u32, "no_target_available"),
            SelectError::RateCapExhausted { retry_after_sec, .. } => {
                (*retry_after_sec, "worker_rate_capped")
            }
        };
        if req.method() == Method::Ack {
            return self.drop_without_target(NoTargetDrop::of(req));
        }
        if matches!(err, SelectError::NoTarget { .. }) && !self.first_view.is_in() {
            return self.drop_without_target(NoTargetDrop::of(req));
        }
        self.refuse_new(req, arrival, retry_after, metric).await
    }
}

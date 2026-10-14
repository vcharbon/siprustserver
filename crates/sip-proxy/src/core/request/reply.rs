//! Self-generated UAS finals: the synthesized response to the packet source,
//! the `ackhop|` absorb memo the proxy writes when it becomes the UAS of a
//! non-2xx INVITE transaction, the refusal of a new request (one 503 for every
//! copy, [`crate::refusals`]), the select-failure → 503 mapping, and the rule
//! that an ACK is never answered (RFC 3261 §17): a reject of an ACK drops it.
//! Relayed responses do NOT live here — see `core/response`.

use std::net::SocketAddr;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::HeaderValue;
use sip_message::types::SipHeader;
use sip_message::{Method, SipRequest, SipResponse};

use crate::observability::metrics::{Direction, MessageResult, RoutingDecisionKind};
use crate::strategy::SelectError;
use crate::trace::emit;

use super::super::ProxyCore;
use super::{top_via_branch, RouteOutcome};

/// A typed value on the response generator's extra-header seam, which still
/// takes wire lines.
pub(super) fn extra_header(value: impl HeaderValue) -> SipHeader {
    let name = value.name();
    SipHeader { name: name.as_wire_str().into(), value: value.to_wire().into() }
}

impl ProxyCore {
    /// Reject `req` locally: answer it with `status`, counted under `metric`.
    /// An ACK is never answered — nothing awaits a response to it — so a
    /// rejected ACK is dropped, counted `ack_not_answered`.
    pub(super) async fn reply_reject(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        status: u16,
        reason: &str,
        extra: &[SipHeader],
        metric: &str,
    ) -> RouteOutcome {
        if req.method() == Method::Ack {
            self.metrics.record_reject("ack_not_answered");
        } else {
            self.reply(req, src, status, reason, extra).await;
            self.metrics.record_reject(metric);
        }
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }

    /// A Request-URI this hop cannot read: 400, counted `malformed_request_uri`.
    pub(super) async fn reply_malformed_uri(
        &self,
        req: &SipRequest,
        src: SocketAddr,
    ) -> RouteOutcome {
        self.reply_reject(req, src, 400, "Bad Request", &[], "malformed_request_uri").await
    }

    /// Refuse the new request `req` with a 503, `retry_after_sec` being the
    /// refusing stage's hint, counted once under `metric`: every later copy
    /// draws the same bytes from [`repeat_refusal`](Self::repeat_refusal).
    pub(super) async fn refuse_new(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        retry_after_sec: u32,
        metric: &str,
    ) -> RouteOutcome {
        let resp = self.refusals.refuse(req, retry_after_sec, self.now_ms());
        self.send_own_final(req, src, &resp).await;
        self.metrics.record_reject(metric);
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }

    /// Answer a copy of a request refused within 64·T1 with that refusal's
    /// bytes, judged and counted no further. `None` when `req` is no such copy.
    pub(super) async fn repeat_refusal(
        &self,
        req: &SipRequest,
        src: SocketAddr,
    ) -> Option<RouteOutcome> {
        let resp = self.refusals.repeat(req, self.now_ms())?;
        self.send_own_final(req, src, &resp).await;
        Some(RouteOutcome { decision: RoutingDecisionKind::Reject, target: None })
    }

    /// Answer 200 to a CANCEL of an INVITE this hop refused within 64·T1: the
    /// proxy is that INVITE's UAS and its final is already sent (RFC 3261
    /// §9.2), so the CANCEL goes no further. `None` when `req` cancels no such
    /// INVITE.
    pub(super) async fn cancel_refused(
        &self,
        req: &SipRequest,
        src: SocketAddr,
    ) -> Option<RouteOutcome> {
        let resp = self.refusals.cancel(req, self.now_ms())?;
        self.send_own_final(req, src, &resp).await;
        Some(RouteOutcome { decision: RoutingDecisionKind::Cancel, target: None })
    }

    /// Synthesize a UAS response to the source.
    async fn reply(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        status: u16,
        reason: &str,
        extra: &[SipHeader],
    ) {
        let opts = GenerateResponseOpts {
            to_tag: Some(self.refusals.to_tag(req)),
            extra_headers: extra.to_vec(),
            ..Default::default()
        };
        let resp = generate_response(req, status, reason, &opts);
        self.send_own_final(req, src, &resp).await;
    }

    /// Send `resp`, the proxy's own final to `req`, to the source.
    async fn send_own_final(&self, req: &SipRequest, src: SocketAddr, resp: &SipResponse) {
        // A recipe freezes into its own image, and that image IS the wire form.
        self.reply_to_source(resp.image(), src).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Responded);
        // This is the proxy's ONLY self-generated-final seam, so it is the only
        // place a traced call's own 503 / 483 / 420 / 400 becomes a `sip.out`.
        // A shed call never reaches a downstream hop, so without this its span
        // would show the INVITE arriving and nothing leaving.
        emit::responded(
            &self.traces,
            req.call_id().as_str(),
            self.now_ms() as i64,
            src,
            resp.image(),
        );

        // ── §16.7 / §17.1.1.3: absorb the ACK to our OWN non-2xx INVITE final ─
        // Generating a final response makes this proxy the UAS of that INVITE
        // transaction, and its non-2xx ACK is hop-by-hop — the upstream's ACK
        // terminates HERE. Write the `ackhop|` memo (see `cancel_lru.rs`) with
        // an EMPTY `branch`, which is what tells the request path to absorb
        // the matching ACK rather than relay it (no downstream exists for a
        // self-generated reject; relaying would run the strategy and hand a
        // worker a stray ACK matching no transaction it ever created).
        if (300..700).contains(&resp.status())
            && req.method() == Method::Invite
            && top_via_branch(req).is_some()
        {
            let from = req.from();
            self.cancel_lru.remember(
                &crate::cancel_lru::ack_hop_key(
                    req.top_via(),
                    req.call_id().as_str(),
                    from.tag(),
                    req.cseq().seq(),
                ),
                crate::cancel_lru::CancelEntry {
                    target: self.advertised.clone(),
                    branch: String::new(),
                    stickiness: None,
                    minted: None,
                    via_cookie: None,
                },
                crate::cancel_lru::RTX_ENTRY_TTL_MS,
            );
        }
    }

    /// Refuse the request a `select_for_new_dialog` failure leaves without a
    /// target. Each failure carries its own `Retry-After` base and reject
    /// label; the 503 itself names neither (no `Reason`). An ACK is dropped.
    pub(super) async fn reply_select_failure(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        err: SelectError,
    ) -> RouteOutcome {
        let (retry_after, metric) = match &err {
            SelectError::NoTarget { .. } => (5u32, "no_target_available"),
            SelectError::RateCapExhausted { retry_after_sec, .. } => {
                (*retry_after_sec, "worker_rate_capped")
            }
        };
        if req.method() == Method::Ack {
            self.metrics.record_reject("ack_not_answered");
            return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
        }
        self.refuse_new(req, src, retry_after, metric).await
    }
}

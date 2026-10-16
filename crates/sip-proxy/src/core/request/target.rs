//! The downstream target of one request, for the routing ladder
//! ([`route`](super::route)): a CANCEL's (where its INVITE went), the
//! Request-URI's, the stickiness cookie's, or a fresh selection's.

use std::net::SocketAddr;

use sip_message::emergency::is_emergency_request;
use sip_message::{SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{invite_txn_key, CancelEntry};
use crate::observability::metrics::{CancelLookup, Face, NoTargetDrop, RoutingDecisionKind};
use crate::strategy::{DecodeResult, RouteParams, SelectOpts};

use super::super::ProxyCore;
use super::RouteOutcome;

/// A target with the decision that chose it and, when a cookie was read, the
/// cookie's verdict for the traced routing fact.
pub(super) type Chosen = (ProxyAddr, RoutingDecisionKind, Option<&'static str>);

impl ProxyCore {
    /// The Request-URI as the target. An out-of-range port is malformed (400),
    /// not truncated: `sip:host:70596` must not be forwarded to port 5060.
    pub(super) async fn request_uri_target(
        &self,
        req: &SipRequest,
        src: SocketAddr,
    ) -> Result<ProxyAddr, RouteOutcome> {
        let uri = req.request_uri();
        if uri.is_opaque() {
            return Err(self.reply_malformed_uri(req, src).await);
        }
        let (host, port) = uri.host_port();
        Ok(ProxyAddr::new(host, port))
    }

    /// A fresh selection. `opts.admit_as_new` (the To-tag rule,
    /// [`ProxyCore::admits_as_new`]): it meets the gate first, as a new call.
    pub(super) async fn select_target(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        src: SocketAddr,
        opts: SelectOpts,
    ) -> Result<ProxyAddr, RouteOutcome> {
        if opts.admit_as_new {
            self.admit_new_call(req, src, is_emergency_request(req)).await?;
        }
        match self.strategy.select_for_new_dialog(msg, opts).await {
            Ok(target) => Ok(target),
            Err(e) => Err(self.reply_select_failure(req, src, e).await),
        }
    }

    /// Where a CANCEL goes. RFC 3261 §9.1 puts it where its INVITE went and
    /// §16.11 on the INVITE's branch, which the survivor's rebuilt INVITE
    /// transaction is keyed by. The CANCEL's Via is its INVITE's top Via, so
    /// it finds the entry of its own INVITE transaction, never another pass of
    /// a spiral sharing its Call-ID, From tag and CSeq. It goes where the
    /// INVITE went while that node is not a worker the proxy reads not alive:
    /// the INVITE's client transaction lives there and only there (§16.10).
    /// Toward a worker no longer alive it follows the INVITE's stickiness
    /// cookie, the workers being ONE logical UAS: the alive primary, else the
    /// backup holding the call's replica (ADR-0014), else nowhere — dropped
    /// and counted (ADR-0050 §5). With no entry, a worker's CANCEL goes to the
    /// Request-URI, where its b-leg INVITE went; the CANCEL of an INVITE this
    /// hop refused is answered 200 here (`Err`, §9.2); any other goes to a
    /// fresh selection.
    pub(super) async fn cancel_target(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        src: SocketAddr,
        worker_outbound: bool,
    ) -> Result<ProxyAddr, RouteOutcome> {
        let key = invite_txn_key(
            req.top_via(),
            req.call_id().as_str(),
            req.from().tag(),
            req.cseq().seq(),
        );
        let Some(found) = self.cancel_lru.lookup(&key) else {
            if worker_outbound {
                self.metrics.record_cancel_lookup(CancelLookup::Miss);
                return self.request_uri_target(req, src).await;
            }
            if let Some(answered) = self.cancel_refused(req, src).await {
                self.metrics.record_cancel_lookup(CancelLookup::Hit);
                return Err(answered);
            }
            self.metrics.record_cancel_lookup(CancelLookup::Miss);
            return self.select_target(msg, req, src, SelectOpts::default()).await;
        };
        self.metrics.record_cancel_lookup(CancelLookup::Hit);
        match self.follow_live_hop(found.target, found.stickiness.as_ref(), msg).await {
            Some(target) => Ok(target),
            None => Err(self.drop_without_target(NoTargetDrop::Cancel)),
        }
    }

    /// The node a request goes to whose transaction already went to `went`:
    /// `went` itself while it is not a worker the proxy reads not alive, else
    /// where `cookie` places the call now. `None`: the cookie verified and
    /// names no alive worker. A cookie the strategy cannot place keeps `went`.
    pub(super) async fn follow_live_hop(
        &self,
        went: ProxyAddr,
        cookie: Option<&RouteParams>,
        msg: &SipMessage,
    ) -> Option<ProxyAddr> {
        let not_alive = self.registry.lookup_by_address(&went).is_some_and(|w| !w.is_alive());
        let Some(cookie) = cookie.filter(|_| not_alive) else {
            return Some(went);
        };
        match self.strategy.decode_stickiness(cookie, msg).await {
            DecodeResult::Forward { target, .. } | DecodeResult::ForwardBackup { target, .. } => {
                Some(target)
            }
            DecodeResult::Unroutable => None,
            DecodeResult::Reject { .. } | DecodeResult::Unknown { .. } => Some(went),
        }
    }

    /// Drop a request no alive worker can take, unanswered: its sender's
    /// retransmission meets the next worker view (ADR-0050 §5).
    pub(super) fn drop_without_target(&self, kind: NoTargetDrop) -> RouteOutcome {
        self.metrics.record_no_target_dropped(kind);
        self.metrics.record_reject("no_target_dropped");
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }

    /// The target `cookie` places the request on. A cookie the strategy
    /// cannot read falls to a fresh selection, which a retransmission
    /// (`rtx_hit`) repeats from its memo instead of rolling the dice again; a
    /// verified cookie naming no alive worker drops the request.
    /// `face`: the request's ingress face, for the To-tag rule
    /// ([`ProxyCore::admits_as_new`]).
    pub(super) async fn cookie_target(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        src: SocketAddr,
        cookie: &RouteParams,
        rtx_hit: Option<&CancelEntry>,
        face: Face,
    ) -> Result<Chosen, RouteOutcome> {
        let decoded = self.strategy.decode_stickiness(cookie, msg).await;
        let miss = match decoded {
            DecodeResult::Forward { target, .. } => {
                return Ok((target, RoutingDecisionKind::DecodeForward, Some("hit")));
            }
            DecodeResult::ForwardBackup { target, .. } => {
                return Ok((target, RoutingDecisionKind::DecodeForwardBackup, Some("backup")));
            }
            DecodeResult::Reject { status, reason } => {
                // The decode reasons are free-form diagnostics — one static
                // label keeps the reason set bounded.
                let metric = "stickiness_decode_rejected";
                return Err(self.reply_reject(req, src, status, &reason, &[], metric).await);
            }
            DecodeResult::Unroutable => {
                return Err(self.drop_without_target(NoTargetDrop::InDialog));
            }
            DecodeResult::Unknown { is_emergency } => is_emergency,
        };
        let target = match rtx_hit {
            Some(found) => found.target.clone(),
            None => {
                let admit_as_new = self.admits_as_new(msg, face, Some(cookie));
                let opts = SelectOpts { emergency_override: miss, admit_as_new };
                self.select_target(msg, req, src, opts).await?
            }
        };
        Ok((target, RoutingDecisionKind::DecodeForward, Some("miss")))
    }
}

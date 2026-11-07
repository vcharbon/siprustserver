//! The downstream target of one request, for the routing ladder
//! ([`route`](super::route)): a CANCEL's (where its INVITE went), a non-2xx
//! ACK's with no ACK hop (where its INVITE went), the Request-URI's, the
//! stickiness cookie's, an admitted transaction's no memo places (ADR-0009
//! X7), or a fresh selection's.

use sip_message::emergency::is_emergency_request;
use sip_message::{SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{invite_txn_key, CancelEntry};
use crate::observability::metrics::{CancelLookup, NoTargetDrop, RoutingDecisionKind};
use crate::strategy::{DecodeResult, RankPlacement, RouteParams, SelectOpts};

use super::super::ProxyCore;
use super::classify::Classified;
use super::{Arrival, RouteOutcome};

/// A target with the decision that chose it and, when a cookie was read, the
/// cookie's verdict for the traced routing fact.
pub(super) type Chosen = (ProxyAddr, RoutingDecisionKind, Option<&'static str>);

impl ProxyCore {
    /// The Request-URI as the target, past §16.4 ([`Classified`]), its `maddr`
    /// where it has one (RFC 3261 §16.5). An out-of-range port is malformed
    /// (400), not truncated: `sip:host:70596` must not be forwarded to port 5060.
    /// A URI of a scheme that names no host (`urn:`, `http:` …) is no target this
    /// hop resolves (416, RFC 3261 §16.3).
    pub(super) async fn request_uri_target(
        &self,
        req: &SipRequest,
        arrival: Arrival,
        c: &Classified,
    ) -> Result<ProxyAddr, RouteOutcome> {
        let uri = c.request_uri_past_own_maddr.as_ref().unwrap_or_else(|| req.request_uri());
        if uri.is_opaque() {
            return Err(self.reply_malformed_uri(req, arrival).await);
        }
        if uri.opaque_part().is_some() {
            return Err(self.reply_unsupported_scheme(req, arrival).await);
        }
        let (host, port) = uri.next_hop();
        Ok(ProxyAddr::new(host, port))
    }

    /// A fresh selection. `opts.admit_as_new` (the To-tag rule,
    /// [`ProxyCore::admits_as_new`]): it meets the gate first, as a new call.
    pub(super) async fn select_target(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        arrival: Arrival,
        opts: SelectOpts,
    ) -> Result<ProxyAddr, RouteOutcome> {
        if opts.admit_as_new {
            self.admit_new_call(req, arrival, is_emergency_request(req)).await?;
        }
        match self.strategy.select_for_new_dialog(msg, opts).await {
            Ok(target) => Ok(target),
            Err(e) => Err(self.reply_select_failure(req, arrival, e).await),
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
    /// and counted (ADR-0050 §5). With no entry (the INVITE crossed another
    /// instance, or the entry was forgotten), the CANCEL of an INVITE this
    /// hop refused is answered 200 here (`Err`, §9.2); any other repeats its
    /// INVITE's Request-URI and Route, so it takes the route the INVITE took
    /// without the memo: its loose Route, a worker's Request-URI, its cookie,
    /// else the rendezvous winner among the alive workers (ADR-0009 X7). No
    /// CANCEL meets an admission gate or draws a 503: with no worker to take
    /// it, it is dropped and counted.
    pub(super) async fn cancel_target(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        arrival: Arrival,
        c: &Classified,
    ) -> Result<ProxyAddr, RouteOutcome> {
        let key = invite_txn_key(
            req.top_via(),
            req.call_id().as_str(),
            req.from().tag(),
            req.cseq().seq(),
        );
        // An INVITE this hop answered itself is cancelled here (§9.2), its
        // hop remembered or not: a 513 follows the forward attempt.
        if let Some(answered) = self.cancel_refused(req, arrival).await {
            self.metrics.record_cancel_lookup(CancelLookup::Hit);
            return Err(answered);
        }
        let Some(found) = self.cancel_lru.lookup(&key) else {
            self.metrics.record_cancel_lookup(CancelLookup::Miss);
            if let Some(next) = &c.loose_route_next_hop {
                return Ok(next.clone());
            }
            if c.worker_outbound {
                return self.request_uri_target(req, arrival, c).await;
            }
            let cookie = c.cookie.as_ref();
            return self.admitted_target(msg, cookie, NoTargetDrop::Cancel).await;
        };
        self.metrics.record_cancel_lookup(CancelLookup::Hit);
        match self.follow_live_hop(found.target, found.stickiness.as_ref(), msg).await {
            Some(target) => Ok(target),
            None => Err(self.drop_without_target(NoTargetDrop::Cancel)),
        }
    }

    /// Where the INVITE went whose non-2xx final the ACK `req` acknowledges,
    /// read off that INVITE's entry, which the ACK's top Via keys (§17.1.1.3:
    /// the ACK repeats the INVITE's, branch included). Toward a worker no
    /// longer alive, where the INVITE's cookie places the call. `None`: no
    /// entry, or no branch to tell the ACK's transaction by.
    pub(super) async fn invite_hop(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
    ) -> Option<Result<ProxyAddr, RouteOutcome>> {
        super::top_via_branch(req)?;
        let key = invite_txn_key(
            req.top_via(),
            req.call_id().as_str(),
            req.from().tag(),
            req.cseq().seq(),
        );
        let found = self.cancel_lru.lookup(&key)?;
        Some(match self.follow_live_hop(found.target, found.stickiness.as_ref(), msg).await {
            Some(target) => Ok(target),
            None => Err(self.drop_without_target(NoTargetDrop::Initial)),
        })
    }

    /// Where a request of an admitted transaction goes when no memo of this
    /// instance places it (ADR-0009 X7), [`admitted_hop`](Self::admitted_hop).
    /// It meets no admission gate and is never answered: with no worker to
    /// take it, it is dropped and counted under `drop`.
    pub(super) async fn admitted_target(
        &self,
        msg: &SipMessage,
        cookie: Option<&RouteParams>,
        drop: NoTargetDrop,
    ) -> Result<ProxyAddr, RouteOutcome> {
        match self.admitted_hop(msg, cookie).await {
            Some(target) => Ok(target),
            None => Err(self.drop_without_target(drop)),
        }
    }

    /// The memo-free hop of a request of an admitted call: where `cookie`
    /// places its dialog, else where the strategy places the call by rank
    /// among the alive workers, with no filter, spill or token. A strategy
    /// that places by no rank selects afresh. `None`: no worker takes it.
    async fn admitted_hop(
        &self,
        msg: &SipMessage,
        cookie: Option<&RouteParams>,
    ) -> Option<ProxyAddr> {
        if let Some(cookie) = cookie {
            match self.strategy.decode_stickiness(cookie, msg).await {
                DecodeResult::Forward { target, .. }
                | DecodeResult::ForwardBackup { target, .. } => return Some(target),
                DecodeResult::Unroutable => return None,
                DecodeResult::Reject { .. } | DecodeResult::Unknown { .. } => {}
            }
        }
        match self.strategy.placement_by_rank(msg) {
            RankPlacement::Node(target) => Some(target),
            RankPlacement::NoNode => None,
            RankPlacement::Unranked => {
                self.strategy.select_for_new_dialog(msg, SelectOpts::default()).await.ok()
            }
        }
    }

    /// The node a request goes to whose transaction already went to `went`:
    /// with no `cookie`, `went` itself; with one, `went` while it is the
    /// address of an alive worker the cookie names, else where the cookie
    /// places the call now — whatever the registry remembers of `went`, a
    /// departed address being forgotten after its window or bound by another
    /// worker since. `None`: the cookie verified and names no alive worker. A
    /// cookie the strategy cannot place keeps `went`.
    pub(super) async fn follow_live_hop(
        &self,
        went: ProxyAddr,
        cookie: Option<&RouteParams>,
        msg: &SipMessage,
    ) -> Option<ProxyAddr> {
        let Some(cookie) = cookie else { return Some(went) };
        let named =
            [self.strategy.stickiness_primary(cookie), self.strategy.stickiness_backup(cookie)];
        let live_there = named
            .into_iter()
            .flatten()
            .filter_map(|id| self.registry.resolve(id))
            .any(|w| w.address == went && w.is_alive());
        if live_there {
            return Some(went);
        }
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
    /// verified cookie naming no alive worker drops the request. The request's
    /// ingress face decides the To-tag rule ([`ProxyCore::admits_as_new`]).
    pub(super) async fn cookie_target(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        arrival: Arrival,
        cookie: &RouteParams,
        rtx_hit: Option<&CancelEntry>,
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
                return Err(self.reply_reject(req, arrival, status, &reason, &[], metric).await);
            }
            DecodeResult::Unroutable => {
                return Err(self.drop_without_target(NoTargetDrop::InDialog));
            }
            DecodeResult::Unknown { is_emergency } => is_emergency,
        };
        let target = match rtx_hit {
            Some(found) => found.target.clone(),
            None => {
                let admit_as_new = self.admits_as_new(msg, arrival.face, Some(cookie));
                let opts = SelectOpts { emergency_override: miss, admit_as_new };
                self.select_target(msg, req, arrival, opts).await?
            }
        };
        Ok((target, RoutingDecisionKind::DecodeForward, Some("miss")))
    }
}

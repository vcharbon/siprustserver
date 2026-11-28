//! §16.4 Route preprocessing and a request's direction, under the trust
//! boundary of the ingress face (ADR-0048).
//!
//! Every leading Route entry naming this proxy is popped. The direction is
//! read from one of them: in a run of whole pairs, the first entry of the last
//! pair; in an odd run, the first entry (why the last pair: ADR-0009 X1). A
//! direction entry carrying `;outbound` is the worker-facing half of the
//! proxy's double Record-Route; any other carries the stickiness cookie.
//!
//! - **Internal face**, and the one face of a single-face proxy: the request
//!   is worker-outbound when its direction entry carries `;outbound`, when its
//!   source is a registry worker, or when its top Via sent-by is one, unless two
//!   or more whole pairs lead its Route and the last one's cookie verifies for
//!   the call: that request re-enters through a later pass. A loose Route past
//!   the proxy's own is its next hop. The three signs are read by
//!   [`WorkerSigns`](crate::worker_signs::WorkerSigns), as the intake shed
//!   reads them.
//! - **External face**: none of these claims holds. Each one is ignored and
//!   counted, every Route entry past the proxy's own is removed and counted,
//!   and the request is routed as an inbound one, by its cookie or by a fresh
//!   selection.
//!
//! On either face, a Request-URI whose `maddr` names this proxy loses it.

use std::net::SocketAddr;

use sip_message::draft::RequestDraft;
use sip_message::header::{RouteEntry, Uri};
use sip_message::{HeaderName, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::headers::{cookie_params, route_target};
use crate::observability::metrics::{Face, UntrustedClaim};
use crate::strategy::RouteParams;

use super::super::{is_dialog_creating, ProxyCore};

/// What Route preprocessing read off one request.
pub(super) struct Classified {
    /// The proxy's own leading Route entries, popped off the draft.
    pub(super) own_routes: Vec<RouteEntry>,
    /// The direction entry's params when the request is routed by its cookie.
    pub(super) cookie: Option<RouteParams>,
    /// The request leaves the internal plane for its Request-URI.
    pub(super) worker_outbound: bool,
    /// The registry worker the top Via names, internal face only: the
    /// originator a worker-outbound dialog's cookie is minted for.
    pub(super) via_worker: Option<ProxyAddr>,
    /// The loose Route past the proxy's own, internal face only.
    pub(super) loose_route_next_hop: Option<ProxyAddr>,
    /// The Request-URI as forwarded, when its `maddr` named this proxy.
    pub(super) request_uri_past_own_maddr: Option<Uri>,
    /// The request re-enters through a later pass, internal face only: its
    /// target is the worker a verified cookie names.
    pub(super) reentry: bool,
}

impl ProxyCore {
    /// Whether `req`, received from `src`, comes from a registry worker: by its
    /// source, or by its top Via where that source is translated.
    pub(super) fn sent_by_a_worker(&self, req: &SipRequest, src: SocketAddr) -> bool {
        self.signs.sent_by_a_worker(req, src)
    }

    /// `uri`, a Request-URI, without its `maddr` when that names this proxy at
    /// the URI's port (RFC 3261 §16.4); `None` when it does not.
    fn past_own_maddr(&self, uri: &Uri) -> Option<Uri> {
        uri.maddr()?;
        let (target, port) = uri.next_hop();
        self.is_self_addr(target, port).then(|| uri.clone().without_own_maddr())
    }

    /// Pop the proxy's own Route entries off `draft` and classify `req`, the
    /// request `msg` holds, received from `src` on `face`. `None`: the draft
    /// refused the pop.
    ///
    /// A route set the strict reader rejects is one this hop cannot act on:
    /// on the internal face it rides through as it arrived, on the external
    /// face it is removed.
    pub(super) fn classify(
        &self,
        msg: &SipMessage,
        req: &SipRequest,
        src: SocketAddr,
        face: Face,
        mut draft: RequestDraft,
    ) -> Option<(RequestDraft, Classified)> {
        let mut routes: Vec<RouteEntry> = req.list::<RouteEntry>().unwrap_or_default();
        let own = self.signs.own_routes(&routes);
        let self_routes = own.count;
        let external = face == Face::External;
        let mut cookie = None;
        let mut worker_outbound = false;
        if let Some(direction_route) = own.direction {
            let params = cookie_params(direction_route.uri());
            if !own.outbound_marker {
                cookie = Some(params);
            } else if external {
                self.metrics.record_external_untrusted(UntrustedClaim::OutboundMarker);
                cookie = Some(params);
            } else {
                worker_outbound = true;
            }
        }
        draft = draft.pop_top_values::<RouteEntry>(self_routes).ok()?;
        let request_uri_past_own_maddr = self.past_own_maddr(req.request_uri());
        if let Some(uri) = &request_uri_past_own_maddr {
            draft = draft.with_uri(uri.clone());
        }

        if external {
            if routes.len() > self_routes || (routes.is_empty() && req.has(&HeaderName::Route)) {
                self.metrics.record_external_untrusted(UntrustedClaim::ForeignRoute);
                draft = draft.remove(&HeaderName::Route);
            }
            if self.signs.worker_source(src) {
                self.metrics.record_external_untrusted(UntrustedClaim::WorkerSource);
            }
            if self.signs.worker_via(req).is_some() {
                self.metrics.record_external_untrusted(UntrustedClaim::WorkerVia);
            }
            routes.truncate(self_routes);
            let classified = Classified {
                own_routes: routes,
                cookie,
                worker_outbound: false,
                via_worker: None,
                loose_route_next_hop: None,
                request_uri_past_own_maddr,
                reentry: false,
            };
            return Some((draft, classified));
        }

        let via_worker = if !worker_outbound || is_dialog_creating(req.method().as_str()) {
            self.signs.worker_via(req)
        } else {
            None
        };
        // ADR-0048 §10
        let spiral_reentry = self_routes > 2
            && self_routes.is_multiple_of(2)
            && cookie.as_ref().is_some_and(|c| self.strategy.verified_cookie(c, msg).is_some());
        if !worker_outbound
            && !spiral_reentry
            && (self.signs.worker_source(src) || via_worker.is_some())
        {
            worker_outbound = true;
            cookie = None;
        }
        let loose_route_next_hop = routes
            .get(self_routes)
            .filter(|next| next.uri().is_loose_route())
            .map(|next| route_target(next.uri()));
        routes.truncate(self_routes);
        let classified = Classified {
            own_routes: routes,
            cookie,
            worker_outbound,
            via_worker,
            loose_route_next_hop,
            request_uri_past_own_maddr,
            reentry: spiral_reentry,
        };
        Some((draft, classified))
    }
}

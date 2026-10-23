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
//!   source is a registry worker, or when its top Via sent-by is one. A loose
//!   Route past the proxy's own is its next hop.
//! - **External face**: none of these claims holds. Each one is ignored and
//!   counted, every Route entry past the proxy's own is removed and counted,
//!   and the request is routed as an inbound one, by its cookie or by a fresh
//!   selection.

use std::net::SocketAddr;

use sip_message::draft::RequestDraft;
use sip_message::header::{RouteEntry, Uri};
use sip_message::{HeaderName, Method, SipRequest};

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
}

impl ProxyCore {
    /// The top-Via sent-by when it is a registry worker. It identifies a
    /// worker whose source address is translated on its way to the proxy.
    fn worker_via_sent_by(&self, req: &SipRequest) -> Option<ProxyAddr> {
        let (host, port) = req.top_via().sent_by().pair();
        let addr = ProxyAddr::new(host, port);
        self.registry.lookup_by_address(&addr).is_some().then_some(addr)
    }

    /// Whether a Route / Record-Route URI names this proxy, on either face.
    fn is_self_route(&self, uri: &Uri) -> bool {
        let (host, port) = uri.host_port();
        self.is_self_addr(host, port)
    }

    /// Pop the proxy's own Route entries off `draft` and classify `req`,
    /// received from `src` on `face`. `None`: the draft refused the pop.
    ///
    /// A route set the strict reader rejects is one this hop cannot act on:
    /// on the internal face it rides through as it arrived, on the external
    /// face it is removed.
    pub(super) fn classify(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        face: Face,
        mut draft: RequestDraft,
    ) -> Option<(RequestDraft, Classified)> {
        let mut routes: Vec<RouteEntry> = req.list::<RouteEntry>().unwrap_or_default();
        let self_routes = routes.iter().take_while(|r| self.is_self_route(r.uri())).count();
        let direction_route = match self_routes {
            0 => None,
            n if n % 2 == 0 => routes.get(n - 2),
            _ => routes.first(),
        };
        let external = face == Face::External;
        let mut cookie = None;
        let mut worker_outbound = false;
        if let Some(direction_route) = direction_route {
            let params = cookie_params(direction_route.uri());
            if !params.contains_key("outbound") {
                cookie = Some(params);
            } else if external {
                self.metrics.record_external_untrusted(UntrustedClaim::OutboundMarker);
                cookie = Some(params);
            } else {
                worker_outbound = true;
            }
        }
        for _ in 0..self_routes {
            // §7.3.1: a UA may fold its whole route set into one comma-combined
            // header, so the pop drops one ENTRY, never the line.
            draft = draft.pop_top::<RouteEntry>().ok()?;
        }

        if external {
            if routes.len() > self_routes || (routes.is_empty() && req.has(&HeaderName::Route)) {
                self.metrics.record_external_untrusted(UntrustedClaim::ForeignRoute);
                draft = draft.remove(&HeaderName::Route);
            }
            if self.registry.lookup_by_address(&ProxyAddr::from(src)).is_some() {
                self.metrics.record_external_untrusted(UntrustedClaim::WorkerSource);
            }
            if self.worker_via_sent_by(req).is_some() {
                self.metrics.record_external_untrusted(UntrustedClaim::WorkerVia);
            }
            routes.truncate(self_routes);
            let classified = Classified {
                own_routes: routes,
                cookie,
                worker_outbound: false,
                via_worker: None,
                loose_route_next_hop: None,
            };
            return Some((draft, classified));
        }

        // A worker-originated in-dialog request (a keepalive OPTIONS toward
        // the caller) carries the proxy's cookie entry with no `;outbound`:
        // its source or its Via names the worker, and that makes it
        // worker-outbound instead of bouncing it back to a worker by the
        // cookie. The Via lookup runs only when something consumes it.
        let via_worker = if !worker_outbound || is_dialog_creating(req.method().as_str()) {
            self.worker_via_sent_by(req)
        } else {
            None
        };
        // A run of two or more whole pairs whose last pair decodes is a request
        // re-entering through a later pass: its worker Via is the earlier
        // pass's origin and does not make it worker-outbound.
        let spiral_reentry = self_routes > 2 && self_routes % 2 == 0 && cookie.is_some();
        if !worker_outbound
            && !spiral_reentry
            && (self.registry.lookup_by_address(&ProxyAddr::from(src)).is_some()
                || via_worker.is_some())
        {
            worker_outbound = true;
            cookie = None;
        }
        let loose_route_next_hop = routes
            .get(self_routes)
            .filter(|next| req.method() != Method::Cancel && next.uri().is_loose_route())
            .map(|next| route_target(next.uri()));
        routes.truncate(self_routes);
        let classified = Classified {
            own_routes: routes,
            cookie,
            worker_outbound,
            via_worker,
            loose_route_next_hop,
        };
        Some((draft, classified))
    }
}

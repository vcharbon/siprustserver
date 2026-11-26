//! The signs that a request was sent by a worker, read the same by the core's
//! Route preprocessing (`core::request::classify`) and by a single-face
//! proxy's intake shed ([`crate::intake_shed`]): its direction Route entry to
//! this proxy carries `;outbound`, its source is a registry worker, or its top
//! Via sent-by is one (a worker whose source is translated on its way here).
//! The internal face, and the one face of a single-face proxy, trusts them;
//! the external face trusts none (ADR-0048).

use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::header::{RouteEntry, Uri};
use sip_message::ip_literal::same_host;
use sip_message::SipRequest;

use crate::addr::ProxyAddr;
use crate::headers::cookie_params;
use crate::registry::WorkerRegistry;

/// This proxy's own addresses and the registry a worker sign is read
/// against. Every read is lock-free: the registry's are (`WorkerRegistry`),
/// and a worker the registry lists later is read as one from then on.
#[derive(Clone)]
pub struct WorkerSigns {
    registry: Arc<dyn WorkerRegistry>,
    advertised: ProxyAddr,
    external: Option<ProxyAddr>,
}

/// The leading run of a request's Route entries naming this proxy.
pub(crate) struct OwnRoutes<'a> {
    /// How many entries lead the Route naming this proxy.
    pub(crate) count: usize,
    /// The entry the direction is read from: in a run of whole pairs, the
    /// first entry of the last pair; in an odd run, the first (ADR-0009 X1).
    pub(crate) direction: Option<&'a RouteEntry>,
    /// The direction entry carries `;outbound`: the worker-facing half of the
    /// proxy's double Record-Route.
    pub(crate) outbound_marker: bool,
}

impl WorkerSigns {
    /// The signs read against `registry`, for a proxy advertising
    /// `advertised` on its internal (or only) face and `external` on its
    /// external face, when it has one.
    pub fn new(
        registry: Arc<dyn WorkerRegistry>,
        advertised: ProxyAddr,
        external: Option<ProxyAddr>,
    ) -> Self {
        Self { registry, advertised, external }
    }

    /// Whether `req`, received from `src` on a face that trusts the signs,
    /// carries one. The core reads a request that does as worker-outbound,
    /// unless it re-enters on a verified cookie (`core::request::classify`).
    pub fn reads_as_a_workers(&self, req: &SipRequest, src: SocketAddr) -> bool {
        let routes: Vec<RouteEntry> = req.list::<RouteEntry>().unwrap_or_default();
        self.own_routes(&routes).outbound_marker || self.sent_by_a_worker(req, src)
    }

    /// Whether `req`, received from `src`, comes from a registry worker: by
    /// its source, or by its top Via where that source is translated.
    pub fn sent_by_a_worker(&self, req: &SipRequest, src: SocketAddr) -> bool {
        self.worker_source(src) || self.worker_via(req).is_some()
    }

    /// Whether `src` is a registry worker's address.
    pub(crate) fn worker_source(&self, src: SocketAddr) -> bool {
        self.registry.lookup_by_address(&ProxyAddr::from(src)).is_some()
    }

    /// The top-Via sent-by when it is a registry worker.
    pub(crate) fn worker_via(&self, req: &SipRequest) -> Option<ProxyAddr> {
        let (host, port) = req.top_via().sent_by().pair();
        let addr = ProxyAddr::new(host, port);
        self.registry.lookup_by_address(&addr).is_some().then_some(addr)
    }

    /// The leading entries of `routes` naming this proxy, and the direction
    /// read off them.
    pub(crate) fn own_routes<'a>(&self, routes: &'a [RouteEntry]) -> OwnRoutes<'a> {
        let count = routes.iter().take_while(|r| self.is_self_route(r.uri())).count();
        let direction = match count {
            0 => None,
            n if n % 2 == 0 => routes.get(n - 2),
            _ => routes.first(),
        };
        let outbound_marker =
            direction.is_some_and(|r| cookie_params(r.uri()).contains_key("outbound"));
        OwnRoutes { count, direction, outbound_marker }
    }

    /// Whether a Route / Record-Route URI names this proxy, on either face, by
    /// its authority or by its next hop: forwarding an entry whose `maddr`
    /// names this proxy would loop on Max-Forwards.
    pub(crate) fn is_self_route(&self, uri: &Uri) -> bool {
        let (host, port) = uri.host_port();
        let (target, target_port) = uri.next_hop();
        self.is_self_addr(host, port) || self.is_self_addr(target, target_port)
    }

    /// Whether `host:port` names this proxy: either face's advertise.
    pub(crate) fn is_self_addr(&self, host: &str, port: u16) -> bool {
        let names = |at: &ProxyAddr| port == at.port && same_host(host, &at.host);
        names(&self.advertised) || self.external.as_ref().is_some_and(names)
    }
}

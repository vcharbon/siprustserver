//! [`Proxy`] — a minimal, *scripted* loose-routing proxy (the test stand-in
//! for the LB front proxy) and its RFC 3261 §16 hop rewrite.

use std::net::SocketAddr;

use sip_message::header::{HeaderName, RecordRouteEntry, RouteEntry, Uri, Via};
use sip_message::{SipMessage, SipRequest, SipResponse};

use super::addressing::{hostport_to_addr, via_addr};
use super::step::unwrap_step;
use super::Agent;

/// A minimal loose-routing proxy. It does the load-bearing routing rewrite
/// per RFC 3261 §16:
///   - adds its own **Via** (top) to forwarded requests so responses route back
///     through it (§16.6), and strips that Via from responses (§16.7);
///   - inserts a `;lr` **Record-Route** (top) on dialog-creating INVITEs so both
///     peers route in-dialog requests through it (§16.6.4);
///   - strips its own top **Route** from in-dialog requests it is the loose
///     router for (§16.4) before forwarding.
///
/// It is *stateless* and *scripted*: the test says which way to forward each
/// message (the real proxy resolves the next hop from the top Route / RURI).
#[derive(Clone)]
pub struct Proxy {
    agent: Agent,
}

impl Proxy {
    pub(super) fn new(agent: Agent) -> Self {
        Proxy { agent }
    }

    pub fn addr(&self) -> SocketAddr {
        self.agent.addr
    }
    pub fn name(&self) -> &str {
        &self.agent.name
    }

    /// The loose-route entry this proxy records on a dialog (§16.6.4).
    fn record_route(&self) -> RecordRouteEntry {
        let uri = Uri::sip(self.agent.addr.ip().to_string())
            .with_port(self.agent.addr.port())
            .with_flag("lr");
        RecordRouteEntry::from_uri(uri)
    }

    /// This proxy's own hop, with a fresh branch (§16.6 step 8).
    fn via(&self) -> Via {
        Via::udp(self.agent.addr.ip().to_string(), self.agent.addr.port())
            .with_branch(self.agent.branch())
    }

    /// Whether the request's first Route entry names this proxy — the
    /// precondition for the §16.4 self-pop.
    fn owns_top_route(&self, req: &SipRequest) -> bool {
        let Ok(routes) = req.route_set() else { return false };
        let Some(top) = routes.first() else { return false };
        let (host, port) = top.uri().host_port();
        hostport_to_addr(&format!("{host}:{port}")) == Some(self.agent.addr)
    }

    /// Receive one request, apply the §16 rewrite, and forward it to `next`.
    /// Returns the (rewritten) request for assertions.
    pub async fn forward_request(&self, next: SocketAddr) -> SipRequest {
        let SipMessage::Request(req) = self.agent.recv().await else {
            panic!("{} expected a request to forward", self.agent.name);
        };
        let mut draft = req.thaw();
        // Loose router popping itself off the route set (§16.4) — in-dialog
        // requests (ACK/BYE/…) arrive with our Record-Route as the top Route.
        if self.owns_top_route(&req) {
            draft =
                draft.pop_top::<RouteEntry>().expect("the top Route just read as a route entry");
        }
        // Record-Route dialog-creating requests so in-dialog traffic returns
        // through us (§16.6.4). A dialog-creating INVITE has no To-tag yet. Ours
        // is the topmost entry, and §7.3 asks a proxy to write what it processes
        // near the top — so a first Record-Route opens the header block.
        if req.method() == "INVITE" && req.to().tag().is_none() {
            let entry = self.record_route();
            draft = if draft.has(&HeaderName::RecordRoute) {
                draft.prepend(entry)
            } else {
                draft.push_front(entry)
            };
        }
        // Add our Via on top so the response comes back to us (§16.6).
        draft = draft.prepend(self.via());
        let forwarded =
            draft.freeze().expect("a thawed request stays complete through a §16 rewrite");
        unwrap_step(self.agent.try_send_wire(forwarded.image(), next).await);
        forwarded
    }

    /// Receive one response, strip our Via, and forward it to `next`.
    pub async fn forward_response(&self, next: SocketAddr) -> SipResponse {
        let SipMessage::Response(resp) = self.agent.recv().await else {
            panic!("{} expected a response to forward", self.agent.name);
        };
        let mut draft = resp.thaw();
        // §16.7 step 3: the response's topmost Via is ours — drop it.
        if via_addr(resp.top_via()) == Some(self.agent.addr) {
            draft = draft.pop_top::<Via>().expect("the top Via parsed on the way in");
        }
        let forwarded =
            draft.freeze().expect("a thawed response stays complete through a §16.7 pop");
        unwrap_step(self.agent.try_send_wire(forwarded.image(), next).await);
        forwarded
    }
}

//! [`Proxy`] — a minimal, *scripted* loose-routing proxy (the test stand-in
//! for the LB front proxy) and its RFC 3261 §16 header surgery.

use std::net::SocketAddr;

use sip_message::generators::strip_route_uri_to_request_uri;
use sip_message::{SipHeader, SipMessage, SipRequest, SipResponse};

use super::addressing::{uri_to_addr, via_addr};
use super::Agent;

/// A minimal loose-routing proxy. It does the load-bearing routing surgery
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

    fn record_route_value(&self) -> String {
        format!("<sip:{}:{};lr>", self.agent.addr.ip(), self.agent.addr.port())
    }

    /// Receive one request, apply the §16 surgery, and forward it to `next`.
    /// Returns the (rewritten) request for assertions.
    pub async fn forward_request(&self, next: SocketAddr) -> SipRequest {
        let SipMessage::Request(mut req) = self.agent.recv().await else {
            panic!("{} expected a request to forward", self.agent.name);
        };
        // Loose router popping itself off the route set (§16.4) — in-dialog
        // requests (ACK/BYE/…) arrive with our Record-Route as the top Route.
        strip_top_route_if_self(&mut req, self.agent.addr);
        // Record-Route dialog-creating requests so in-dialog traffic returns
        // through us (§16.6.4). A dialog-creating INVITE has no To-tag yet.
        if req.method == "INVITE" && req.to.tag.is_none() {
            prepend_header(&mut req.headers, "Record-Route", &self.record_route_value());
        }
        // Add our Via on top so the response comes back to us (§16.6).
        prepend_header(&mut req.headers, "Via", &self.via_value());
        self.agent.send(&SipMessage::Request(req.clone()), next).await;
        req
    }

    /// Receive one response, strip our Via, and forward it to `next`.
    pub async fn forward_response(&self, next: SocketAddr) -> SipResponse {
        let SipMessage::Response(mut resp) = self.agent.recv().await else {
            panic!("{} expected a response to forward", self.agent.name);
        };
        strip_top_via_if_self(&mut resp.headers, self.agent.addr);
        self.agent.send(&SipMessage::Response(resp.clone()), next).await;
        resp
    }

    fn via_value(&self) -> String {
        format!(
            "SIP/2.0/UDP {}:{};branch={}",
            self.agent.addr.ip(),
            self.agent.addr.port(),
            self.agent.branch()
        )
    }
}

/// Insert a header at the top of the list (RFC 3261 §16.6 prepend semantics for
/// Via / Record-Route).
fn prepend_header(headers: &mut Vec<SipHeader>, name: &str, value: &str) {
    headers.insert(
        0,
        SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        },
    );
}

/// Strip the first Route header if it routes to `me` (the loose router removing
/// itself, §16.4).
fn strip_top_route_if_self(req: &mut SipRequest, me: SocketAddr) {
    if let Some(pos) = req
        .headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("route"))
    {
        let uri = strip_route_uri_to_request_uri(&req.headers[pos].value);
        if uri_to_addr(&uri) == Some(me) {
            req.headers.remove(pos);
        }
    }
}

/// Strip the topmost Via if it is `me`'s (the proxy removing its own Via from a
/// response before forwarding upstream, §16.7).
fn strip_top_via_if_self(headers: &mut Vec<SipHeader>, me: SocketAddr) {
    if let Some(pos) = headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("via"))
    {
        if via_addr(&headers[pos].value) == Some(me) {
            headers.remove(pos);
        }
    }
}

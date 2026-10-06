//! [`Proxy`] — a minimal, *scripted* loose-routing proxy (the test stand-in
//! for the LB front proxy) — and the RFC 3261 §16 hop rewrite any proxy
//! fixture applies ([`forwarded_request`], [`forwarded_response`] and their
//! next hops).

use std::net::SocketAddr;

use sip_message::header::{HeaderName, MaxForwards, RecordRouteEntry, RouteEntry, Uri, Via};
use sip_message::hops::forwarded_max_forwards;
use sip_message::{SipMessage, SipRequest, SipResponse};

use super::addressing::{hostport_to_addr, via_addr};
use super::step::unwrap_step;
use super::Agent;

/// A minimal loose-routing proxy. It does the load-bearing routing rewrite
/// per RFC 3261 §16 ([`forwarded_request`] / [`forwarded_response`]).
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

    /// Receive one request, apply the §16 rewrite, and forward it to `next`.
    /// Returns the (rewritten) request for assertions.
    pub async fn forward_request(&self, next: SocketAddr) -> SipRequest {
        self.forward_request_altered(next, |req| req).await
    }

    /// [`Self::forward_request`] with `alter` applied to the rewritten request
    /// before it leaves: how a test scripts a proxy that corrupts what it
    /// forwards.
    pub async fn forward_request_altered(
        &self,
        next: SocketAddr,
        alter: impl FnOnce(SipRequest) -> SipRequest,
    ) -> SipRequest {
        let SipMessage::Request(req) = self.agent.recv().await else {
            panic!("{} expected a request to forward", self.agent.name);
        };
        let forwarded = alter(forwarded_request(&req, self.agent.addr, &self.agent.branch()));
        unwrap_step(self.agent.try_send_wire(forwarded.image(), next).await);
        forwarded
    }

    /// Receive one response, strip our Via, and forward it to `next`.
    pub async fn forward_response(&self, next: SocketAddr) -> SipResponse {
        self.forward_response_altered(next, |resp| resp).await
    }

    /// [`Self::forward_response`] with `alter` applied to the rewritten
    /// response before it leaves.
    pub async fn forward_response_altered(
        &self,
        next: SocketAddr,
        alter: impl FnOnce(SipResponse) -> SipResponse,
    ) -> SipResponse {
        let SipMessage::Response(resp) = self.agent.recv().await else {
            panic!("{} expected a response to forward", self.agent.name);
        };
        let forwarded = alter(forwarded_response(&resp, self.agent.addr));
        unwrap_step(self.agent.try_send_wire(forwarded.image(), next).await);
        forwarded
    }
}

/// `req` as the proxy at `hop` forwards it on `branch` (RFC 3261 §16.4, §16.6):
///   - its own top Route popped, when it names `hop` (the loose-router pop);
///   - Max-Forwards decremented, or added at 70 where the request states none
///     (step 3);
///   - a `;lr` Record-Route for `hop` on a dialog-creating INVITE (step 4),
///     topmost, so both peers route in-dialog requests through it;
///   - `hop`'s Via on top with `branch` (step 8), so responses return to it.
///
/// From, To, Call-ID, CSeq and the body ride unchanged.
pub fn forwarded_request(req: &SipRequest, hop: SocketAddr, branch: &str) -> SipRequest {
    let mut draft = req.thaw();
    if top_route_addr(req) == Some(hop) {
        draft = draft.pop_top::<RouteEntry>().expect("the top Route just read as a route entry");
    }
    // A dialog-creating INVITE has no To-tag yet. Ours is the topmost entry,
    // and §7.3 asks a proxy to write what it processes near the top — so a
    // first Record-Route opens the header block.
    if req.method() == "INVITE" && req.to().tag().is_none() {
        let uri = Uri::sip(hop.ip().to_string()).with_port(hop.port()).with_flag("lr");
        let entry = RecordRouteEntry::from_uri(uri);
        draft = if draft.has(&HeaderName::RecordRoute) {
            draft.prepend(entry)
        } else {
            draft.push_front(entry)
        };
    }
    let hops = match req.header::<MaxForwards>() {
        None => MaxForwards::DEFAULT,
        Some(_) => forwarded_max_forwards(req),
    };
    draft
        .set(hops)
        .prepend(Via::udp(hop.ip().to_string(), hop.port()).with_branch(branch.to_string()))
        .freeze()
        .expect("a thawed request stays complete through a §16 rewrite")
}

/// `resp` as the proxy at `hop` forwards it upstream: its topmost Via, when it
/// is `hop`'s, popped (§16.7 step 3).
pub fn forwarded_response(resp: &SipResponse, hop: SocketAddr) -> SipResponse {
    let mut draft = resp.thaw();
    if via_addr(resp.top_via()) == Some(hop) {
        draft = draft.pop_top::<Via>().expect("the top Via parsed on the way in");
    }
    draft.freeze().expect("a thawed response stays complete through a §16.7 pop")
}

/// Where a forwarded request goes (§16.6 steps 6-7): its first Route entry,
/// else its Request-URI.
pub fn request_next_hop(req: &SipRequest) -> Option<SocketAddr> {
    top_route_addr(req).or_else(|| {
        let (host, port) = req.request_uri().host_port();
        hostport_to_addr(&format!("{host}:{port}"))
    })
}

/// Where a forwarded response goes (§18.2.2): the sent-by of its topmost Via.
pub fn response_next_hop(resp: &SipResponse) -> Option<SocketAddr> {
    via_addr(resp.top_via())
}

/// The address the request's first Route entry names, if it has one.
fn top_route_addr(req: &SipRequest) -> Option<SocketAddr> {
    let routes = req.route_set().ok()?;
    let (host, port) = routes.first()?.uri().host_port();
    hostport_to_addr(&format!("{host}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;

    const HOP: &str = "127.0.0.1:5090";

    fn invite(max_forwards: Option<&str>) -> SipRequest {
        let hops = max_forwards.map(|v| format!("Max-Forwards: {v}\r\n")).unwrap_or_default();
        let raw = format!(
            "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a-1\r\n\
             {hops}\
             From: <sip:alice@127.0.0.1>;tag=a1\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: hops@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()) {
            Ok(SipMessage::Request(r)) => r,
            other => panic!("not a request: {other:?}"),
        }
    }

    fn hops(req: &SipRequest) -> u32 {
        req.header::<MaxForwards>().expect("stated").expect("readable").value()
    }

    /// §16.6 step 3: the received count, less one.
    #[test]
    fn a_stated_hop_count_is_decremented() {
        let forwarded = forwarded_request(&invite(Some("12")), HOP.parse().unwrap(), "z9hG4bK-p");
        assert_eq!(hops(&forwarded), 11);
    }

    /// §16.6 step 3: a request stating no count gets one, of 70.
    #[test]
    fn a_missing_hop_count_is_added_at_70() {
        let forwarded = forwarded_request(&invite(None), HOP.parse().unwrap(), "z9hG4bK-p");
        assert_eq!(hops(&forwarded), 70);
    }
}

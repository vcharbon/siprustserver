//! The server transport's two halves of one rule: what it records on the top
//! Via of a request it receives (RFC 3261 §18.2.1, RFC 3581 §4), and where
//! every response to that request goes (§18.2.2). A stack that stamps on
//! receipt and answers at [`response_destination`] sends every response of a
//! request to the one address the stamp names, whatever the status.

use std::net::SocketAddr;

use crate::header::Via;
use crate::ip_literal::ip_socket_addr;
use crate::types::SipRequest;

/// The host and port a Via records for a packet from `src`: an IPv4-mapped
/// IPv6 address is its IPv4 address.
pub fn observed_source(src: SocketAddr) -> (String, u16) {
    (src.ip().to_canonical().to_string(), src.port())
}

/// `req` with its top Via stamped from `src` ([`Via::stamped_from`]). The
/// request comes back as it came, not re-rendered, when the stamp changes
/// nothing; otherwise it is rendered once with the top Via line rewritten,
/// which normalises the spacing of the lines around it as any render does
/// (the bytes that arrived stay the caller's to keep). A line the render
/// cannot write whole or read back leaves the request as it came.
pub fn stamp_received(req: SipRequest, src: SocketAddr) -> SipRequest {
    let (host, port) = observed_source(src);
    let top = req.top_via();
    if top.clone().stamped_from(&host, port) == *top {
        return req;
    }
    let stamped = req
        .thaw()
        .update_top::<Via>(|via| via.stamped_from(&host, port))
        .ok()
        .and_then(|draft| draft.refreeze_received().ok());
    stamped.unwrap_or(req)
}

/// The socket address a response answering a request whose top Via is `via`
/// goes to over unreliable unicast: its [`response_target`](Via::response_target).
/// `None` when that host is a name, which only a resolver can turn into an
/// address.
pub fn response_destination(via: &Via) -> Option<SocketAddr> {
    let (host, port) = via.response_target();
    ip_socket_addr(host, port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CustomParser, SipMessage, SipParser};

    /// A received request lacking the Max-Forwards this stack puts on what
    /// it mints: the stamp still applies.
    fn request(via: &str) -> SipRequest {
        let raw = format!(
            "OPTIONS sip:b@192.0.2.1 SIP/2.0\r\n\
             Via: {via}\r\n\
             Via: SIP/2.0/UDP 198.51.100.9:5060;branch=z9hG4bK-lower\r\n\
             From: <sip:a@192.0.2.2>;tag=a\r\n\
             To: <sip:b@192.0.2.1>\r\n\
             Call-ID: c\r\n\
             CSeq: 1 OPTIONS\r\n\
             Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()) {
            Ok(SipMessage::Request(r)) => r,
            other => panic!("a request: {other:?}"),
        }
    }

    fn src(s: &str) -> SocketAddr {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn a_hop_seen_from_its_sent_by_comes_back_untouched() {
        let req = request("SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-1");
        let image = req.image().clone();
        let stamped = stamp_received(req, src("192.0.2.2:5060"));
        assert_eq!(stamped.image().as_ptr(), image.as_ptr(), "not re-rendered");
    }

    #[test]
    fn a_bare_rport_is_filled_and_received_added() {
        let req = request("SIP/2.0/UDP 10.0.0.5:5060;branch=z9hG4bK-1;rport");
        let stamped = stamp_received(req, src("192.0.2.2:40000"));
        let text = String::from_utf8_lossy(stamped.image()).to_string();
        assert!(
            text.contains("Via: SIP/2.0/UDP 10.0.0.5:5060;branch=z9hG4bK-1;rport=40000;received=192.0.2.2\r\n"),
            "{text}"
        );
        assert!(text.contains("Via: SIP/2.0/UDP 198.51.100.9:5060;branch=z9hG4bK-lower\r\n"));
        assert_eq!(response_destination(stamped.top_via()), Some(src("192.0.2.2:40000")));
    }

    #[test]
    fn another_host_without_rport_is_answered_at_received_and_the_sent_by_port() {
        let req = request("SIP/2.0/UDP 10.0.0.5:5070;branch=z9hG4bK-1");
        let stamped = stamp_received(req, src("192.0.2.2:40000"));
        assert_eq!(stamped.top_via().received(), Some("192.0.2.2"));
        assert_eq!(response_destination(stamped.top_via()), Some(src("192.0.2.2:5070")));
    }

    #[test]
    fn a_v4_mapped_source_is_recorded_as_its_v4_address() {
        assert_eq!(
            observed_source(src("[::ffff:192.0.2.7]:5070")),
            ("192.0.2.7".to_string(), 5070)
        );
        assert_eq!(observed_source(src("[2001:db8::1]:5070")), ("2001:db8::1".to_string(), 5070));
        let req = request("SIP/2.0/UDP 192.0.2.7:5070;branch=z9hG4bK-1");
        let image = req.image().clone();
        let stamped = stamp_received(req, src("[::ffff:192.0.2.7]:5070"));
        assert_eq!(stamped.image().as_ptr(), image.as_ptr(), "the same address: untouched");
    }

    #[test]
    fn a_named_sent_by_is_answered_at_received() {
        let req = request("SIP/2.0/UDP alice.invalid:5060;branch=z9hG4bK-1");
        assert_eq!(response_destination(req.top_via()), None, "a name needs a resolver");
        let stamped = stamp_received(req, src("192.0.2.2:5060"));
        assert_eq!(response_destination(stamped.top_via()), Some(src("192.0.2.2:5060")));
    }
}

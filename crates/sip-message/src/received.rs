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

/// A request as the server transport hands it on ([`stamp_received`]).
#[derive(Debug, Clone)]
pub struct Received {
    /// The request, its top Via stamped when the stamp could be written.
    pub request: SipRequest,
    /// Where every response to it goes: the response target of its top Via
    /// as stamped (§18.2.2), whether or not the stamp could be written.
    pub destination: SocketAddr,
    /// What became of the stamp.
    pub stamp: Stamp,
}

/// What [`stamp_received`] did to the request's top Via.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stamp {
    /// The stamp changes nothing: the request is the one received, not
    /// re-rendered.
    Unchanged,
    /// The request was rendered once with its top Via line rewritten.
    Written,
    /// The render refused a line, or the top Via line does not read: the
    /// request is the one received, and only `destination` holds the stamp.
    Unwritten,
}

/// `req`, received from `src`, with its top Via stamped from it
/// ([`Via::stamped_from`]), and the destination of every response to it.
/// The stamp is computed once, on the hop alone, so the destination holds
/// even where the stamp cannot be written. Writing it renders the request
/// once: the top Via line is rewritten from its parse, lower hops folded on
/// that line with it, and every other line is written from its parse as any
/// render does (the bytes that arrived stay the caller's to keep).
pub fn stamp_received(req: SipRequest, src: SocketAddr) -> Received {
    let (host, port) = observed_source(src);
    let stamped = req.top_via().clone().stamped_from(&host, port);
    let destination = response_destination(&stamped).unwrap_or(src);
    if stamped == *req.top_via() {
        return Received { request: req, destination, stamp: Stamp::Unchanged };
    }
    let written = req
        .thaw()
        .update_top::<Via>(|_| stamped)
        .ok()
        .and_then(|draft| draft.refreeze_received().ok());
    match written {
        Some(request) => Received { request, destination, stamp: Stamp::Written },
        None => Received { request: req, destination, stamp: Stamp::Unwritten },
    }
}

/// Where every response to a request whose top Via is `via`, received from
/// `src`, goes: the response target of that hop as stamped from `src`
/// (§18.2.1, §18.2.2, RFC 3581 §4), read off the hop without stamping the
/// request. `src` where the target names no IP address.
pub fn stamped_destination(via: &Via, src: SocketAddr) -> SocketAddr {
    let (host, port) = observed_source(src);
    response_destination(&via.clone().stamped_from(&host, port)).unwrap_or(src)
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
        let got = stamp_received(req, src("192.0.2.2:5060"));
        assert_eq!(got.stamp, Stamp::Unchanged);
        assert_eq!(got.request.image().as_ptr(), image.as_ptr(), "not re-rendered");
        assert_eq!(got.destination, src("192.0.2.2:5060"));
    }

    #[test]
    fn a_bare_rport_is_filled_and_received_added() {
        let req = request("SIP/2.0/UDP 10.0.0.5:5060;branch=z9hG4bK-1;rport");
        let got = stamp_received(req, src("192.0.2.2:40000"));
        assert_eq!(got.stamp, Stamp::Written);
        let text = String::from_utf8_lossy(got.request.image()).to_string();
        assert!(
            text.contains("Via: SIP/2.0/UDP 10.0.0.5:5060;branch=z9hG4bK-1;rport=40000;received=192.0.2.2\r\n"),
            "{text}"
        );
        assert!(text.contains("Via: SIP/2.0/UDP 198.51.100.9:5060;branch=z9hG4bK-lower\r\n"));
        assert_eq!(response_destination(got.request.top_via()), Some(got.destination));
        assert_eq!(got.destination, src("192.0.2.2:40000"));
    }

    #[test]
    fn another_host_without_rport_is_answered_at_received_and_the_sent_by_port() {
        let req = request("SIP/2.0/UDP 10.0.0.5:5070;branch=z9hG4bK-1");
        let got = stamp_received(req, src("192.0.2.2:40000"));
        assert_eq!(got.request.top_via().received(), Some("192.0.2.2"));
        assert_eq!(got.destination, src("192.0.2.2:5070"));
    }

    #[test]
    fn a_v4_mapped_source_is_recorded_as_its_v4_address() {
        assert_eq!(
            observed_source(src("[::ffff:192.0.2.7]:5070")),
            ("192.0.2.7".to_string(), 5070)
        );
        assert_eq!(observed_source(src("[2001:db8::1]:5070")), ("2001:db8::1".to_string(), 5070));
        let req = request("SIP/2.0/UDP 192.0.2.7:5070;branch=z9hG4bK-1");
        let got = stamp_received(req, src("[::ffff:192.0.2.7]:5070"));
        assert_eq!(got.stamp, Stamp::Unchanged, "the same address");
    }

    #[test]
    fn a_named_sent_by_is_answered_at_received() {
        let req = request("SIP/2.0/UDP alice.invalid:5060;branch=z9hG4bK-1");
        assert_eq!(response_destination(req.top_via()), None, "a name needs a resolver");
        assert_eq!(
            stamped_destination(req.top_via(), src("192.0.2.2:5060")),
            src("192.0.2.2:5060")
        );
        let got = stamp_received(req, src("192.0.2.2:5060"));
        assert_eq!(response_destination(got.request.top_via()), Some(src("192.0.2.2:5060")));
    }

    /// A hop no reader accepts folded on the top Via line, which the lenient
    /// grammar lets through (ADR-0053 §6): the line cannot be rewritten.
    #[test]
    fn a_stamp_that_cannot_be_written_still_names_the_destination() {
        let raw = "OPTIONS sip:b@192.0.2.1 SIP/2.0\r\n\
                   Via: SIP/2.0/UDP 10.0.0.5:5060;branch=z9hG4bK-1;rport, garbage\r\n\
                   From: <sip:a@192.0.2.2>;tag=a\r\n\
                   To: <sip:b@192.0.2.1>\r\n\
                   Call-ID: c\r\n\
                   CSeq: 1 OPTIONS\r\n\
                   Content-Length: 0\r\n\r\n";
        let lenient = CustomParser::with_limits(crate::SipParserLimits {
            wire_grammar: false,
            ..Default::default()
        });
        let Ok(SipMessage::Request(req)) = lenient.parse(raw.as_bytes()) else {
            panic!("the lenient reader takes it");
        };
        let image = req.image().clone();
        let got = stamp_received(req, src("192.0.2.2:40000"));
        assert_eq!(got.stamp, Stamp::Unwritten);
        assert_eq!(got.request.image(), &image, "the request as it came");
        assert_eq!(got.destination, src("192.0.2.2:40000"), "the stamp's target all the same");
    }
}

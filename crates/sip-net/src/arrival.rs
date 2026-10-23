//! One datagram on the receive path, before it is queued: its bytes, and the
//! parse a pre-ingress hook made of them. The parse is made at most once and
//! travels with the queued [`UdpPacket`], so the consumer takes it instead of
//! parsing the datagram again.

use std::net::SocketAddr;

use sip_message::{Bytes, SipParser};

use crate::parse_guard::{parse_datagram, ParseOutcome};
use crate::types::UdpPacket;

/// A received datagram and, once a hook asked for it, its parse. The parse
/// is boxed: a message is a few kilobytes, and the packet that carries it
/// through the queue stays a few words.
#[derive(Debug)]
pub struct Arrival {
    raw: Bytes,
    parsed: Option<Box<ParseOutcome>>,
}

impl Arrival {
    /// The datagram `raw`, not parsed yet.
    pub fn new(raw: Bytes) -> Self {
        Self { raw, parsed: None }
    }

    /// The datagram's bytes.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// The datagram's parse: made by `parser` on the first call, with a
    /// parser panic contained to it, and returned unchanged by every later
    /// call whatever parser it names. The message shares the datagram's
    /// buffer.
    pub fn parse(&mut self, parser: &dyn SipParser) -> &ParseOutcome {
        let raw = &self.raw;
        self.parsed.get_or_insert_with(|| {
            Box::new(parse_datagram(raw.clone(), |b| parser.parse_shared(b)))
        })
    }

    /// The parse a hook made, if one did.
    pub fn parsed(&self) -> Option<&ParseOutcome> {
        self.parsed.as_deref()
    }

    /// The queued packet: the datagram from `src` stamped `arrival_ms`, with
    /// its parse when one was made.
    pub fn into_packet(self, src: SocketAddr, arrival_ms: u64) -> UdpPacket {
        UdpPacket { raw: self.raw, src, arrival_ms, parsed: self.parsed }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sip_message::{CustomParser, SipMessage, SipParseError};

    use super::*;

    const OPTIONS: &[u8] = b"OPTIONS sip:bob@192.0.2.20 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-arrival\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:alice@example.com>;tag=a1\r\n\
        To: <sip:bob@example.com>\r\n\
        Call-ID: arrival@192.0.2.10\r\n\
        CSeq: 1 OPTIONS\r\n\
        Content-Length: 0\r\n\r\n";

    #[derive(Default)]
    struct Counting(AtomicUsize);

    impl SipParser for Counting {
        fn name(&self) -> &str {
            "counting"
        }

        fn parse(&self, raw: &[u8]) -> Result<SipMessage, SipParseError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            CustomParser::new().parse(raw)
        }
    }

    fn src() -> SocketAddr {
        SocketAddr::from(([192, 0, 2, 10], 5060))
    }

    #[test]
    fn a_datagram_is_parsed_once_and_its_parse_travels_with_the_packet() {
        let parser = Counting::default();
        let mut arrival = Arrival::new(Bytes::from_static(OPTIONS));
        assert!(arrival.parsed().is_none());
        assert!(matches!(arrival.parse(&parser), Ok(Ok(SipMessage::Request(_)))));
        assert!(matches!(arrival.parse(&parser), Ok(Ok(SipMessage::Request(_)))));
        assert_eq!(parser.0.load(Ordering::Relaxed), 1, "one parse per datagram");
        let packet = arrival.into_packet(src(), 7);
        assert_eq!(&packet.raw[..], OPTIONS);
        assert!(matches!(packet.parsed.as_deref(), Some(Ok(Ok(SipMessage::Request(_))))));
    }

    #[test]
    fn a_datagram_no_hook_parsed_is_queued_unparsed() {
        let packet = Arrival::new(Bytes::from_static(OPTIONS)).into_packet(src(), 7);
        assert_eq!(packet.parsed, None);
    }
}

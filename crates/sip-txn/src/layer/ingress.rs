//! The owner's inbound packet dispatch: a datagram's parse — the one its
//! receive path made, else one made here with a parser panic contained to it
//! ([`sip_net::parse_guard`]) — a refused one answered or dropped
//! (`parse_refusal`), and the message handed to the server or the client side.

use std::sync::atomic::Ordering::Relaxed;

use sip_message::SipMessage;
use sip_net::{parse_datagram, ParserPanicked, UdpEndpoint, UdpPacket};

use super::owner::Owner;

impl Owner {
    pub(super) async fn handle_packet(&mut self, endpoint: &dyn UdpEndpoint, packet: UdpPacket) {
        let UdpPacket { raw, src, parsed, .. } = packet;
        // The receive buffer is handed to the parser, not lent: the message's
        // `raw`/`body` then share it and no packet byte is copied.
        let parsed = match parsed {
            Some(parsed) => *parsed,
            None => parse_datagram(raw, |b| self.parser.parse_shared(b)),
        };
        let parsed = match parsed {
            Ok(Ok(m)) => m,
            Ok(Err(error)) => {
                self.refuse_unparsed(endpoint, &error, src).await;
                return;
            }
            Err(ParserPanicked) => {
                self.metrics.parse_panics.fetch_add(1, Relaxed);
                return;
            }
        };

        match parsed {
            SipMessage::Request(req) => self.handle_inbound_request(endpoint, req, src).await,
            SipMessage::Response(resp) => self.handle_inbound_response(endpoint, resp, src).await,
        }
    }
}

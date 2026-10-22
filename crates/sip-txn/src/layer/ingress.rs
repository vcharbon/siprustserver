//! The owner's inbound packet dispatch: a datagram is parsed, a parser panic
//! contained to it ([`crate::parse_guard`]), a refused one answered or dropped
//! (`parse_refusal`), and the message handed to the server or the client side.

use std::sync::atomic::Ordering::Relaxed;

use sip_message::SipMessage;
use sip_net::UdpEndpoint;

use crate::parse_guard::{parse_datagram, ParserPanicked};

use super::owner::Owner;

impl Owner {
    pub(super) async fn handle_packet(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        packet: sip_net::UdpPacket,
    ) {
        let src = packet.src;
        // Hand the receive buffer to the parser instead of lending it: the
        // message's `raw`/`body` then share it and no packet byte is copied.
        let raw = bytes::Bytes::from(packet.raw);
        let parsed = match parse_datagram(raw, |raw| self.parser.parse_shared(raw)) {
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

//! The peer final an a-facing answer delivers, reduced to the header lines of
//! it that ride onto the answer the back-to-back UA mints (RFC 3261 §16.6).

use sip_message::generators::{self, RelayScope, SourceBody};
use sip_message::{SipHeader, SipResponse};

/// The lines of a peer final that ride onto the a-facing answer delivering it.
/// Built only from the response itself, by the one relay rule every response
/// exit shares ([`generators::relayable_headers`]), so an action can never state
/// a relay of what it did not receive. [`Self::none`] is an answer the B2BUA
/// gives on its own behalf: no peer final, nothing rides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayedFinal {
    headers: Vec<SipHeader>,
}

impl RelayedFinal {
    /// An answer minted on the B2BUA's own behalf — nothing rides.
    pub const fn none() -> Self {
        Self { headers: Vec::new() }
    }

    /// What of `delivered` rides, `body` stating what the answer carries where
    /// the final had a body: the same octets, a staged replacement, or none.
    pub fn of(delivered: &SipResponse, body: SourceBody) -> Self {
        Self {
            headers: generators::relayable_headers(
                delivered.headers(),
                RelayScope::response_carrying(body),
            ),
        }
    }

    /// The lines that ride, in the final's wire order, repeats kept.
    pub fn headers(&self) -> &[SipHeader] {
        &self.headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    /// A callee 200 stating privacy, an identity line of its own, a vendor
    /// header, its capability advert, plus lines the stack owns or withholds.
    fn callee_200() -> SipResponse {
        let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bK-b\r\n\
Record-Route: <sip:proxy.bob.example;lr>\r\n\
From: <sip:alice@a.example>;tag=a1\r\n\
To: <sip:bob@b.example>;tag=b1\r\n\
Call-ID: b-leg\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5060>\r\n\
Privacy: none\r\n\
P-Identifier: 112233368\r\n\
X-Vendor-Thing: opaque-42\r\n\
Allow: INVITE, ACK, BYE\r\n\
Session-Expires: 1800;refresher=uas\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 4\r\n\r\nv=0\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Response(r) => r,
            _ => panic!("expected a response"),
        }
    }

    fn names(relayed: &RelayedFinal) -> Vec<String> {
        relayed.headers().iter().map(|h| h.name.to_ascii_lowercase()).collect()
    }

    /// The end-to-end lines ride; the structural set, the Contact and a
    /// per-leg negotiation do not.
    #[test]
    fn a_delivered_final_contributes_exactly_its_relayable_lines() {
        let carried = names(&RelayedFinal::of(&callee_200(), SourceBody::Verbatim));
        for name in ["privacy", "p-identifier", "x-vendor-thing", "allow"] {
            assert!(carried.contains(&name.to_string()), "{name} must ride: {carried:?}");
        }
        let stays = [
            "via",
            "record-route",
            "from",
            "to",
            "call-id",
            "cseq",
            "contact",
            "content-type",
            "session-expires",
        ];
        for name in stays {
            assert!(!carried.contains(&name.to_string()), "{name} must not ride: {carried:?}");
        }
    }

    /// An answer on the B2BUA's own behalf relays nothing.
    #[test]
    fn none_relays_nothing() {
        assert!(RelayedFinal::none().headers().is_empty());
        assert_eq!(RelayedFinal::none(), RelayedFinal::default());
    }
}

//! The key of the To-tags the proxy's own finals carry: derived from the
//! cookie key every proxy of a deployment shares (ADR-0046), so a proxy that
//! restarted, or took the VIP over, recognises the ACK to a final another
//! incarnation sent.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sip_txn::{IdGen, StatelessTagger};

/// The label the To-tag key is derived under, keeping it apart from every
/// other use of the cookie key.
const LABEL: &[u8] = b"sip-proxy/own-final-to-tag/v1";

/// The tagger of the proxy's own finals, keyed by HMAC-SHA256 of a fixed
/// label under `cookie_key`. A key HMAC refused (none is) would leave the
/// tagger keyed by this instance's own secret, drawn from `id_gen`.
pub fn own_final_tagger(cookie_key: &[u8], id_gen: &IdGen) -> StatelessTagger {
    derived_key(cookie_key)
        .and_then(|key| StatelessTagger::keyed(&key))
        .unwrap_or_else(|| StatelessTagger::from_id_gen(id_gen))
}

fn derived_key(cookie_key: &[u8]) -> Option<[u8; 32]> {
    let mut mac = Hmac::<Sha256>::new_from_slice(cookie_key).ok()?;
    mac.update(LABEL);
    Some(mac.finalize().into_bytes().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_key_derives_one_pinned_tag_and_another_key_another() {
        let parse = |raw: &str| match sip_message::CustomParser::new().parse(raw.as_bytes()) {
            Ok(sip_message::SipMessage::Request(r)) => r,
            _ => panic!("a request"),
        };
        use sip_message::SipParser;
        let invite = parse(
            "INVITE sip:bob@192.0.2.9 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bKk\r\n\
             From: <sip:a@192.0.2.1>;tag=a\r\nTo: <sip:bob@192.0.2.9>\r\nCall-ID: k@192.0.2.1\r\n\
             CSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n",
        );
        let id_gen = IdGen::seeded(1);
        let tag = |key: &[u8]| own_final_tagger(key, &id_gen).for_request(&invite).0;
        assert_eq!(tag(b"0123456789abcdef-one"), tag(b"0123456789abcdef-one"));
        assert_ne!(tag(b"0123456789abcdef-one"), tag(b"0123456789abcdef-two"));
        // Pinned: every proxy of a deployment, whatever its build, derives
        // this tag, so a change of derivation shows here before a rolling
        // upgrade stops absorbing the other proxies' ACKs.
        assert_eq!(tag(b"0123456789abcdef-golden"), "b362222fb49239cb");
    }
}

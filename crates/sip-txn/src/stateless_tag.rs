//! The identity a stateless UAS answers a request with: a To-tag (and a jitter
//! roll) derived from the request itself, so every copy of one request draws
//! the same answer (RFC 3261 §8.2.7).

use sip_ids::SigningKey;
use sip_message::header::{CSeq, CallId, From, Via};
use sip_message::{Method, RequestPartial, SipRequest};

use crate::rng::IdGen;

/// Derives a transactionless answer's identity from the request, with the
/// keyed construction of RFC 3261 §19.3: HMAC-SHA256 under a secret key over
/// the request's identity, so the tags are the same wherever the key is, in
/// any build, and unguessable elsewhere. Clone shares the key.
#[derive(Debug, Clone)]
pub struct StatelessTagger {
    key: SigningKey,
}

impl StatelessTagger {
    /// A tagger keyed by this instance's own secret, 32 bytes of four tags
    /// drawn from `id_gen`, for an answerer that shares no key.
    pub fn from_id_gen(id_gen: &IdGen) -> Self {
        let mut secret = [0u8; 32];
        let drawn = (0..4).flat_map(|_| id_gen.new_tag().into_bytes());
        for (slot, byte) in secret.iter_mut().zip(drawn) {
            *slot = byte;
        }
        #[expect(clippy::expect_used, reason = "32 bytes is inside SigningKey's key range")]
        let key = SigningKey::new(&secret).expect("a 32-byte key");
        Self { key }
    }

    /// A tagger keyed by `secret` (16 to 64 bytes, [`SigningKey::new`]):
    /// every instance given the same secret derives the same tags, across
    /// restarts, hosts and builds. `None` for a secret of another length.
    pub fn keyed(secret: &[u8]) -> Option<Self> {
        SigningKey::new(secret).map(|key| Self { key })
    }

    /// The `(To-tag, roll)` for `req`: a keyed hash over Call-ID, From-tag,
    /// top-`Via` branch and CSeq, the fields RFC 3261 §19.3 names for a
    /// request-derived tag. Both come from the one hash, so the tag and a
    /// `Retry-After` jittered by the roll are equally stable across copies of
    /// the request and equally spread across distinct requests.
    pub fn for_request(&self, req: &SipRequest) -> (String, u64) {
        self.derive(req.call_id(), req.from(), req.via().first(), req.cseq())
    }

    /// The `(To-tag, roll)` for the refused request `partial` holds, derived
    /// as [`for_request`](Self::for_request) derives it.
    pub fn for_partial(&self, partial: &RequestPartial) -> (String, u64) {
        self.derive(partial.call_id(), partial.from(), partial.via().first(), partial.cseq())
    }

    /// Whether the ACK `ack` acknowledges a final to an INVITE whose To-tag
    /// this tagger derived: the ACK of a non-2xx final shares its INVITE's
    /// Call-ID, From-tag, top-Via branch and CSeq number (§17.1.1.3).
    pub fn acknowledges_own_final(&self, ack: &SipRequest) -> bool {
        let invite = CSeq::new(ack.cseq().seq(), Method::Invite);
        let (tag, _) = self.derive(ack.call_id(), ack.from(), ack.via().first(), &invite);
        ack.to().tag() == Some(tag.as_str())
    }

    fn derive(&self, call_id: &CallId, from: &From, top_via: &Via, cseq: &CSeq) -> (String, u64) {
        let signature = self.key.sign(&[
            call_id.as_str().as_bytes(),
            from.tag().unwrap_or("").as_bytes(),
            top_via.branch().unwrap_or("").as_bytes(),
            &cseq.seq().to_le_bytes(),
            cseq.method().as_str().as_bytes(),
        ]);
        let mut first = [0u8; 8];
        for (slot, byte) in first.iter_mut().zip(signature) {
            *slot = byte;
        }
        let key = u64::from_le_bytes(first);
        (format!("{key:016x}"), key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::{CustomParser, SipMessage, SipParser};

    fn invite_with(call_id: &str, branch: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@caller.test>;tag=alice-tag\r\n\
To: <sip:bob@uas.test>\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5555>\r\n\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    fn invite() -> SipRequest {
        invite_with("tag-test@10.0.0.1", "z9hG4bK-tag")
    }

    /// RFC 3261 §8.2.7: the same request draws the same identity every time.
    #[test]
    fn the_identity_repeats_for_the_same_request() {
        let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(7));
        let first = tagger.for_request(&invite());
        assert_eq!(first, tagger.clone().for_request(&invite()));
        assert!(!first.0.is_empty(), "the derived tag is non-empty");
    }

    /// Distinct requests get distinct tags and distinct rolls.
    #[test]
    fn the_identity_differs_across_requests() {
        let tagger = StatelessTagger::from_id_gen(&IdGen::seeded(7));
        let one = tagger.for_request(&invite_with("call-a@10.0.0.1", "z9hG4bK-a"));
        let two = tagger.for_request(&invite_with("call-b@10.0.0.1", "z9hG4bK-b"));
        assert_ne!(one.0, two.0);
        assert_ne!(one.1, two.1);
    }

    /// The same request under another instance's secret yields another tag, so
    /// a tag is not guessable from the request alone.
    #[test]
    fn the_tag_is_keyed_by_the_instance_secret() {
        let a = StatelessTagger::from_id_gen(&IdGen::seeded(7)).for_request(&invite());
        let b = StatelessTagger::from_id_gen(&IdGen::seeded(99)).for_request(&invite());
        assert_ne!(a, b);
    }
}

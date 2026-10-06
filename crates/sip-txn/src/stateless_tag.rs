//! The identity a stateless UAS answers a request with: a To-tag (and a jitter
//! roll) derived from the request itself, so every copy of one request draws
//! the same answer (RFC 3261 §8.2.7).

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

use sip_message::SipRequest;

use crate::rng::IdGen;

/// Derives a transactionless answer's identity from the request, with the
/// keyed construction of RFC 3261 §19.3: a per-instance `secret` mixed with
/// the request's identity. Clone shares the secret.
#[derive(Debug, Clone)]
pub struct StatelessTagger {
    secret: u64,
}

impl StatelessTagger {
    /// Draw the instance secret: one tag's worth of entropy from `id_gen`,
    /// hashed to a `u64` and fixed for this tagger's life, so the derived tags
    /// are stable here and unguessable elsewhere.
    pub fn from_id_gen(id_gen: &IdGen) -> Self {
        let mut h = DefaultHasher::new();
        h.write(id_gen.new_tag().as_bytes());
        Self { secret: h.finish() }
    }

    /// The `(To-tag, roll)` for `req`: a keyed hash over Call-ID, From-tag,
    /// top-`Via` branch and CSeq, the fields RFC 3261 §19.3 names for a
    /// request-derived tag. Both come from the one hash, so the tag and a
    /// `Retry-After` jittered by the roll are equally stable across copies of
    /// the request and equally spread across distinct requests.
    pub fn for_request(&self, req: &SipRequest) -> (String, u64) {
        let mut h = DefaultHasher::new();
        h.write_u64(self.secret);
        h.write(req.call_id().as_str().as_bytes());
        h.write(req.from().tag().unwrap_or("").as_bytes());
        h.write(req.via().first().branch().unwrap_or("").as_bytes());
        h.write_u32(req.cseq().seq());
        h.write(req.cseq().method().as_str().as_bytes());
        let key = h.finish();
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

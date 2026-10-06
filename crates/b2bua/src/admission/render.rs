//! The one answer to a refused new INVITE, whatever rung refused it: a 503
//! with a To-tag and a jittered `Retry-After` both derived from the request,
//! so every copy of one INVITE draws the same bytes (RFC 3261 §8.2.7), and no
//! `Reason` (ADR-0037 item 3). A rung ahead of any transaction sends it
//! statelessly and remembers the refusal in the node's memo
//! ([`InviteRefusals`]); a rung behind the INVITE server transaction sends it
//! through that transaction, which absorbs retransmissions and the ACK.

use std::sync::Arc;

use load_shed::retry_after;
use sip_message::generators::{generate_response, CapabilitySet, GenerateResponseOpts};
use sip_message::types::SipHeader;
use sip_message::{SipRequest, SipResponse};
use sip_txn::{IdGen, InviteRefusals, StatelessTagger};
use std::sync::OnceLock;

use super::ladder::Refused;

/// The worker's refusals of new INVITEs: the renderer every rung answers
/// with, and the memo the stateless rungs share. Clone shares both.
#[derive(Clone)]
pub struct Refusals {
    render: Arc<Render>,
    memo: InviteRefusals,
}

struct Render {
    tagger: StatelessTagger,
    retry_after_base_sec: u32,
    retry_after_jitter_sec: u32,
    /// The deployment's advertisement for a minted 503, stated once by the
    /// core that serves under these refusals ([`Refusals::advertise`]).
    advertisement: OnceLock<Vec<SipHeader>>,
}

impl Render {
    /// The 503 refusing `req`, its `Retry-After` jittered over the larger of
    /// the configured base and `not_before_sec`, floored once.
    fn answer(&self, req: &SipRequest, not_before_sec: u32) -> SipResponse {
        let (to_tag, roll) = self.tagger.for_request(req);
        let base = self.retry_after_base_sec.max(not_before_sec);
        let retry_after_sec = retry_after::jittered(base, self.retry_after_jitter_sec, || roll);
        generate_response(
            req,
            503,
            "Service Unavailable",
            &GenerateResponseOpts {
                to_tag: Some(to_tag),
                extra_headers: std::iter::once(SipHeader {
                    name: "Retry-After".to_string().into(),
                    value: retry_after_sec.to_string().into(),
                })
                .chain(self.advertisement.get().into_iter().flatten().cloned())
                .collect(),
                ..Default::default()
            },
        )
    }
}

impl Refusals {
    /// Refusals under the configured `Retry-After` base and jitter,
    /// remembering at most `memo_capacity` refused INVITEs. The To-tag secret
    /// is drawn from `id_gen` once, here.
    pub fn new(
        retry_after_base_sec: u32,
        retry_after_jitter_sec: u32,
        memo_capacity: usize,
        id_gen: &IdGen,
    ) -> Self {
        let render = Arc::new(Render {
            tagger: StatelessTagger::from_id_gen(id_gen),
            retry_after_base_sec,
            retry_after_jitter_sec,
            advertisement: OnceLock::new(),
        });
        let stateless = render.clone();
        let memo = InviteRefusals::with_capacity(
            Arc::new(move |req: &SipRequest| stateless.answer(req, 0)),
            memo_capacity,
        );
        Self { render, memo }
    }

    /// The memo and stateless answer the arrival brake and the transaction
    /// layer share.
    pub fn memo(&self) -> &InviteRefusals {
        &self.memo
    }

    /// State the deployment's `advertisement` for a minted final on every
    /// 503 these refusals answer from now on. The first statement stands: the
    /// refusals serve one worker, whose deployment does not change.
    pub fn advertise(&self, advertisement: &CapabilitySet) {
        let _ = self.render.advertisement.set(
            advertisement
                .lines()
                .into_iter()
                .map(|(name, value)| SipHeader {
                    name: name.as_wire_str().to_string().into(),
                    value: value.into(),
                })
                .collect(),
        );
    }

    /// The answer to `req`, refused by a rung for `refused`.
    pub fn answer(&self, req: &SipRequest, refused: Refused) -> SipResponse {
        self.render.answer(req, refused.not_before_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_calls::Refusal;
    use sip_message::{serialize, CustomParser, SipMessage, SipParser};

    fn invite(branch: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@caller.test>;tag=alice-tag\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: {branch}@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5555>\r\n\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    fn wire(resp: SipResponse) -> String {
        String::from_utf8(serialize(&SipMessage::Response(resp))).expect("utf-8")
    }

    fn refused(reason: Refusal, not_before_sec: u32) -> Refused {
        Refused { reason, not_before_sec }
    }

    /// A tagged 503 echoing the request, with a `Retry-After` and no
    /// `Reason` (ADR-0037 item 3).
    #[test]
    fn the_refusal_is_a_tagged_503_with_retry_after_and_no_reason() {
        let r = Refusals::new(5, 0, 16, &IdGen::seeded(1));
        let out = wire(r.answer(&invite("z9hG4bK-a"), refused(Refusal::CapShed, 0)));
        assert!(out.starts_with("SIP/2.0 503 Service Unavailable\r\n"), "{out}");
        assert!(out.contains("Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-a\r\n"), "{out}");
        assert!(out.contains("Call-ID: z9hG4bK-a@10.0.0.1\r\n"), "{out}");
        assert!(out.contains("CSeq: 1 INVITE\r\n"), "{out}");
        assert!(out.contains("Retry-After: 5\r\n"), "{out}");
        assert!(!out.contains("Reason:"), "{out}");
        let to = out.lines().find(|l| l.starts_with("To:")).expect("a To line");
        assert!(to.contains(";tag="), "{to}");
    }

    /// A base of 0 never asks the caller to retry at once (RFC 3261 §20.33).
    #[test]
    fn the_refusal_never_carries_retry_after_zero() {
        let r = Refusals::new(0, 0, 16, &IdGen::seeded(1));
        let out = wire(r.memo().answer(&invite("z9hG4bK-a")));
        assert!(out.contains("Retry-After: 1\r\n"), "{out}");
    }

    /// Every copy of one INVITE draws a byte-identical answer, jitter
    /// included, from the memo and from a rung that refused it for the
    /// configured base alike; distinct calls get distinct tags.
    #[test]
    fn every_copy_of_one_invite_draws_the_same_refusal() {
        let r = Refusals::new(5, 30, 16, &IdGen::seeded(1));
        let first = wire(r.memo().answer(&invite("z9hG4bK-a")));
        assert_eq!(first, wire(r.clone().memo().answer(&invite("z9hG4bK-a"))));
        assert_eq!(first, wire(r.answer(&invite("z9hG4bK-a"), refused(Refusal::CapacityCalls, 0))));
        assert_ne!(first, wire(r.memo().answer(&invite("z9hG4bK-b"))));
    }

    /// The bucket's time to a token raises the base the jitter spreads from.
    #[test]
    fn a_later_admission_raises_the_retry_after_base() {
        let r = Refusals::new(5, 0, 16, &IdGen::seeded(1));
        let out = wire(r.answer(&invite("z9hG4bK-a"), refused(Refusal::BucketEmpty, 60)));
        assert!(out.contains("Retry-After: 60\r\n"), "{out}");
        let out = wire(r.answer(&invite("z9hG4bK-a"), refused(Refusal::BucketEmpty, 2)));
        assert!(out.contains("Retry-After: 5\r\n"), "{out}");
    }

    /// A refusal is a final minted in the worker's own name: once the core
    /// states the deployment's advertisement, every 503 carries it.
    #[test]
    fn a_refusal_carries_the_advertisement_the_core_stated() {
        use sip_message::header::AcceptRange;
        let r = Refusals::new(5, 0, 16, &IdGen::seeded(1));
        let before = wire(r.answer(&invite("z9hG4bK-adv0"), refused(Refusal::BucketEmpty, 0)));
        assert!(!before.contains("Accept:"), "silent until stated: {before}");
        r.advertise(&CapabilitySet::stating(
            None,
            None,
            Some(vec![AcceptRange::new("application/sdp")]),
        ));
        let after = wire(r.answer(&invite("z9hG4bK-adv1"), refused(Refusal::BucketEmpty, 0)));
        assert!(after.contains("Accept: application/sdp\r\n"), "{after}");
    }
}

//! The worker's ceilings on the transaction layer's deferred backlog and the
//! 503 a refused INVITE draws past them (ADR-0037 item 6).

use std::sync::Arc;

use sip_message::SipRequest;
use sip_txn::{DeferredBound, IdGen};

use crate::capacity::build_capacity_reject_503;
use crate::overload::{jittered_retry_after, StatelessRejectTagger};

/// Deferred events at which a new non-emergency call is refused, in output
/// queues' worth: one full queue waiting behind the full queue.
const NORMAL_QUEUES: usize = 1;
/// Deferred events at which an emergency call and an INVITE carrying a
/// To-tag are refused too, in output queues' worth.
const EMERGENCY_QUEUES: usize = 2;

/// The ceilings for an output queue of `event_capacity` events, refusing with
/// the capacity 503 under the configured `Retry-After` base and jitter. The
/// To-tag secret is drawn from `id_gen` once, here.
pub fn deferred_bound(
    event_capacity: usize,
    retry_after_base_sec: u32,
    retry_after_jitter_sec: u32,
    id_gen: &IdGen,
) -> DeferredBound {
    let tagger = StatelessRejectTagger::from_id_gen(id_gen);
    DeferredBound {
        normal: event_capacity * NORMAL_QUEUES,
        emergency: event_capacity * EMERGENCY_QUEUES,
        refusal: Arc::new(move |req: &SipRequest| {
            let (to_tag, roll) = tagger.for_request(req);
            let retry_after =
                jittered_retry_after(retry_after_base_sec, retry_after_jitter_sec, || roll);
            build_capacity_reject_503(to_tag, req, retry_after)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn wire(bound: &DeferredBound, req: &SipRequest) -> String {
        String::from_utf8(serialize(&SipMessage::Response((bound.refusal)(req)))).expect("utf-8")
    }

    #[test]
    fn the_ceilings_are_one_and_two_queues_of_backlog() {
        let bound = deferred_bound(4096, 5, 0, &IdGen::seeded(1));
        assert_eq!((bound.normal, bound.emergency), (4096, 8192));
    }

    /// The capacity 503 (ADR-0037): tagged, a `Retry-After`, no `Reason`.
    #[test]
    fn the_refusal_is_the_capacity_503() {
        let out = wire(&deferred_bound(64, 5, 0, &IdGen::seeded(1)), &invite("z9hG4bK-a"));
        assert!(out.starts_with("SIP/2.0 503 Service Unavailable\r\n"), "{out}");
        assert!(out.contains("Retry-After: 5\r\n"), "{out}");
        assert!(!out.contains("Reason:"), "{out}");
        let to = out.lines().find(|l| l.starts_with("To:")).expect("a To line");
        assert!(to.contains(";tag="), "{to}");
    }

    /// A base of 0 never asks the caller to retry at once (RFC 3261 §20.33).
    #[test]
    fn the_refusal_never_carries_retry_after_zero() {
        let out = wire(&deferred_bound(64, 0, 0, &IdGen::seeded(1)), &invite("z9hG4bK-a"));
        assert!(out.contains("Retry-After: 1\r\n"), "{out}");
    }

    /// No transaction holds a refused INVITE, so its retransmission is refused
    /// afresh and must draw a byte-identical answer (RFC 3261 §8.2.7); distinct
    /// calls get distinct tags.
    #[test]
    fn every_copy_of_one_invite_draws_the_same_refusal() {
        let bound = deferred_bound(64, 5, 30, &IdGen::seeded(1));
        let first = wire(&bound, &invite("z9hG4bK-a"));
        assert_eq!(first, wire(&bound, &invite("z9hG4bK-a")));
        assert_ne!(first, wire(&bound, &invite("z9hG4bK-b")));
    }
}

//! The single "reject a new call" primitive: the stateless 503 every overload
//! tier sends, and the jittered `Retry-After` it carries.
//!
//! Both tiers of the brake shed the same thing — a NEW, non-emergency call —
//! and therefore emit the same response: a 503 built on
//! [`generate_response`], echoing the INVITE's Via/From/To/Call-ID/CSeq, with
//!   - a fresh **To-tag** (RFC 3261 §8.2.6.2 — this codebase tags every
//!     non-100 final; the RFC audit gate flags a tagless one),
//!   - `Reason: SIP;cause=503;text="overload"` — the overload cause token,
//!     distinct from the readiness 503's `not-ready` / `draining`,
//!   - `Retry-After: <seconds>` — [`jittered_retry_after`] of the configured
//!     base, so a shed fleet does not re-storm in lockstep.
//!
//! "Stateless" is a *call-layer* property: no call, dialog, CDR or limiter
//! state is born for a reject. Tier-1 replies from the pre-ingress hook before
//! the datagram is ever queued; Tier-3 replies through the INVITE server
//! transaction. The peer's ACK matches no dialog either way and is absorbed.

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::types::SipHeader;
use sip_txn::IdGen;

/// Compute a jittered `Retry-After` value (seconds).
///
/// Randomness is **injected**: `roll` yields a fresh value in
/// `[0, u64::MAX]`, keeping this function deterministic and unit-testable.
///
/// Returns `base_sec` unchanged when `jitter_sec == 0`, otherwise
/// `base_sec + (roll % (jitter_sec + 1))` — a uniform offset in the inclusive
/// range `[0, jitter_sec]`.
pub fn jittered_retry_after(base_sec: u32, jitter_sec: u32, roll: impl FnOnce() -> u64) -> u32 {
    if jitter_sec == 0 {
        return base_sec;
    }
    // `jitter_sec + 1` fits in u64 (jitter_sec: u32); the modulus is in
    // [0, jitter_sec] so the sum cannot exceed base_sec + jitter_sec.
    let offset = (roll() % (u64::from(jitter_sec) + 1)) as u32;
    base_sec + offset
}

/// Build the **503 Service Unavailable** that refuses a new call under
/// overload — the one reject shape shared by the Tier-1 ingress brake and the
/// Tier-3 admission gate. `retry_after_sec` is the caller's hint (bucket
/// time-to-token, or [`jittered_retry_after`] of the configured base).
pub fn build_reject_new_call_503(
    id_gen: &IdGen,
    req: &sip_message::SipRequest,
    retry_after_sec: u32,
) -> sip_message::SipResponse {
    generate_response(
        req,
        503,
        "Service Unavailable",
        &GenerateResponseOpts {
            to_tag: Some(id_gen.new_tag()),
            extra_headers: vec![
                SipHeader {
                    name: "Reason".to_string().into(),
                    value: "SIP;cause=503;text=\"overload\"".to_string().into(),
                },
                SipHeader {
                    name: "Retry-After".to_string().into(),
                    value: retry_after_sec.to_string().into(),
                },
            ],
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod jitter_tests {
    //! Pins [`jittered_retry_after`]: the zero-jitter identity path and the
    //! injected-roll modular arithmetic.

    use super::jittered_retry_after;

    #[test]
    fn zero_jitter_returns_base_unchanged() {
        // No randomness: the roll closure is never invoked.
        let mut rolled = false;
        let v = jittered_retry_after(2, 0, || {
            rolled = true;
            999
        });
        assert_eq!(v, 2);
        assert!(!rolled, "zero jitter must not consult the roll source");
    }

    #[test]
    fn roll_is_reduced_modulo_jitter_plus_one() {
        // base=10, jitter=4 → offset ∈ [0, 4]; roll=7 ⇒ 7 % 5 = 2 ⇒ 12.
        assert_eq!(jittered_retry_after(10, 4, || 7), 12);
        // roll exactly at a multiple of (jitter+1) ⇒ offset 0 ⇒ base.
        assert_eq!(jittered_retry_after(10, 4, || 5), 10);
        // roll = jitter ⇒ max offset ⇒ base + jitter.
        assert_eq!(jittered_retry_after(10, 4, || 4), 14);
    }

    #[test]
    fn offset_is_bounded_to_zero_through_jitter_inclusive() {
        let (base, jitter) = (30u32, 6u32);
        for roll in 0u64..50 {
            let v = jittered_retry_after(base, jitter, || roll);
            assert!(
                (base..=base + jitter).contains(&v),
                "roll={roll} produced {v}, outside [{base}, {}]",
                base + jitter
            );
        }
    }

    #[test]
    fn large_roll_does_not_overflow() {
        // u64::MAX % (jitter+1) is still a small offset — no panic, in-range.
        let v = jittered_retry_after(1, 9, || u64::MAX);
        assert!((1..=10).contains(&v));
    }
}

#[cfg(test)]
mod reject_503_tests {
    //! Pins the shared reject: status line, echoed request headers, the
    //! To-tag, and the overload `Reason` / `Retry-After` trailer.

    use super::build_reject_new_call_503;
    use sip_message::{serialize, SipMessage, SipParser, SipRequest};
    use sip_txn::IdGen;

    fn invite() -> SipRequest {
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-reject\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: reject-test@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5555>\r\n\
Content-Length: 0\r\n\r\n";
        match sip_message::CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    fn wire(retry_after_sec: u32) -> String {
        let resp = build_reject_new_call_503(&IdGen::seeded(7), &invite(), retry_after_sec);
        String::from_utf8(serialize(&SipMessage::Response(resp))).expect("utf-8 wire")
    }

    #[test]
    fn rejects_with_a_503_echoing_the_request_dialog_headers() {
        let out = wire(5);
        assert!(out.starts_with("SIP/2.0 503 Service Unavailable\r\n"), "{out}");
        assert!(out.contains("Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-reject\r\n"), "{out}");
        assert!(out.contains("From: <sip:alice@flooder.test>;tag=alice-tag\r\n"), "{out}");
        assert!(out.contains("Call-ID: reject-test@10.0.0.1\r\n"), "{out}");
        assert!(out.contains("CSeq: 1 INVITE\r\n"), "{out}");
    }

    #[test]
    fn the_reject_carries_a_to_tag() {
        let out = wire(5);
        let to_line = out.lines().find(|l| l.starts_with("To:")).expect("a To line");
        assert!(to_line.contains(";tag="), "reject must tag To (RFC 3261 §8.2.6.2): {to_line}");
    }

    #[test]
    fn the_reject_carries_the_overload_reason_and_retry_after() {
        let out = wire(30);
        assert!(out.contains("Reason: SIP;cause=503;text=\"overload\"\r\n"), "{out}");
        assert!(out.contains("Retry-After: 30\r\n"), "{out}");
    }
}

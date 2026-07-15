//! Read/rewrite helpers over SIP messages — accessors on already-parsed
//! headers plus the strict pre-parse byte classifiers that run before the
//! parser. Constructing *new* messages lives in [`crate::generators`];
//! lenient raw-field extraction lives in [`crate::sniff`].
//!
//! Concern map:
//!   - [`headers`] — lookup / set / remove on a parsed header list
//!   - [`name_addr`] — From/To/Contact value readers (tag, URI)
//!   - [`uri`] — SIP-URI string parsing (host, port, params)
//!   - [`via`] — Via value readers (branch + B2BUA `cr`/`lg` params)
//!   - [`param_codec`] — percent-codec for B2BUA correlation params
//!   - [`emergency`] — emergency-call classification (parsed + raw buffer)
//!   - [`preparse`] — strict byte classifiers (INVITE line, To-tag presence)
//!   - [`reject_503`] — stateless Tier-1 overload 503 templating

mod bytes;
pub mod emergency;
pub mod headers;
pub mod name_addr;
pub mod param_codec;
pub mod preparse;
pub mod reject_503;
pub mod uri;
pub mod via;

pub use emergency::{buffer_has_emergency_marker, is_emergency_request};
pub use headers::{get_header, get_headers, remove_header, set_header, split_top_level_commas};
pub use name_addr::{extract_contact_uri, extract_name_addr_uri, extract_tag, strip_tag};
pub use param_codec::{decode_param, encode_param};
pub use preparse::{buffer_has_to_tag, is_invite_request_buffer};
pub use reject_503::{build_stateless_reject_503_buffer, jittered_retry_after};
pub use uri::{extract_host_port, parse_sip_uri, parse_uri_params, ParsedSipUri};
pub use via::{parse_via_params, ViaParams};

#[cfg(test)]
mod brake_composition_tests {
    //! The Tier-1 brake predicate composed from the pure helpers exactly as
    //! the UDP pre-ingress hook evaluates it: at/above the queue threshold,
    //! shed a non-emergency INVITE with a stateless 503; accept everything
    //! else (emergency INVITEs, non-INVITEs, below-threshold traffic).

    use super::{
        buffer_has_emergency_marker, build_stateless_reject_503_buffer, is_invite_request_buffer,
        jittered_retry_after,
    };

    const B2BUA_IP: &str = "127.0.0.1";
    const B2BUA_PORT: u16 = 5060;
    const FLOODER_IP: &str = "10.0.0.1";
    const FLOODER_PORT: u16 = 5555;
    // tier1_threshold = floor(queue_max * threshold_pct / 100) = floor(5 * 40 / 100).
    const TIER1_THRESHOLD: usize = 2;
    const RETRY_AFTER_BASE: u32 = 5;
    const RETRY_AFTER_JITTER: u32 = 0;

    fn invite_buf(i: u32, emergency: bool) -> Vec<u8> {
        let mut s = format!(
            "INVITE sip:bob@{B2BUA_IP}:{B2BUA_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-brake-{i}\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: brake-test-{i}@{FLOODER_IP}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{FLOODER_IP}:{FLOODER_PORT}>\r\n\
Max-Forwards: 70\r\n"
        );
        if emergency {
            s.push_str("Resource-Priority: esnet.0\r\n");
        }
        s.push_str("Content-Length: 0\r\n\r\n");
        s.into_bytes()
    }

    fn options_buf(i: u32) -> Vec<u8> {
        format!(
            "OPTIONS sip:bob@{B2BUA_IP}:{B2BUA_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-opts-{i}\r\n\
From: <sip:alice@flooder.test>;tag=opt-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: opts-{i}@{FLOODER_IP}\r\n\
CSeq: 1 OPTIONS\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// The Tier-1 pre-ingress predicate. Returns the 503 buffer to reply
    /// with, or `None` to accept/enqueue. `depth` is the current inbound
    /// queue depth. A malformed buffer that fails templating falls through
    /// to accept (the normal pipeline rejects it).
    fn brake_decision(raw: &[u8], depth: usize) -> Option<Vec<u8>> {
        if depth >= TIER1_THRESHOLD
            && is_invite_request_buffer(raw)
            && !buffer_has_emergency_marker(raw)
        {
            let retry_after = jittered_retry_after(RETRY_AFTER_BASE, RETRY_AFTER_JITTER, || 0);
            return build_stateless_reject_503_buffer(raw, retry_after);
        }
        None
    }

    #[test]
    fn non_emergency_invites_past_the_threshold_receive_a_stateless_503() {
        // Flood 10 INVITEs into an undrained queue: depths 0 and 1 are below
        // threshold (accepted); depth 2 and every one after is rejected.
        let flood = 10usize;
        let mut rejects = 0;
        for i in 0..flood {
            // No drain → depth equals the count already accepted, capped.
            let depth = i.min(TIER1_THRESHOLD);
            match brake_decision(&invite_buf(i as u32, false), depth) {
                Some(resp) => {
                    rejects += 1;
                    assert!(resp.starts_with(b"SIP/2.0 503"));
                }
                None => assert!(i < TIER1_THRESHOLD, "INVITE {i} below threshold must be accepted"),
            }
        }
        assert_eq!(rejects, flood - TIER1_THRESHOLD);
    }

    #[test]
    fn emergency_invites_bypass_the_brake_even_above_the_threshold() {
        assert!(brake_decision(&invite_buf(0, false), 0).is_none()); // accepted
        assert!(brake_decision(&invite_buf(1, false), 1).is_none()); // accepted
        // At depth == threshold a non-emergency INVITE is shed...
        assert!(brake_decision(&invite_buf(99, false), 2).is_some());
        // ...but the emergency INVITE bypasses via buffer_has_emergency_marker.
        assert!(
            brake_decision(&invite_buf(2, true), 2).is_none(),
            "emergency INVITE must bypass the brake"
        );
    }

    #[test]
    fn non_invite_requests_are_not_503d_by_the_brake() {
        // With the queue saturated, INVITEs are shed but an OPTIONS is still
        // accepted — the brake only targets new INVITEs.
        for i in 2..5u32 {
            assert!(brake_decision(&invite_buf(i, false), 2).is_some());
        }
        assert!(
            brake_decision(&options_buf(0), 2).is_none(),
            "non-INVITE must not be 503'd"
        );
    }
}

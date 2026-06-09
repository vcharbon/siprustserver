//! Port of `tests/harness/rules/rfc/rfc3264-peer-rules.ts` — per-message peer
//! rules that strict-parse the SDP body carried on every **sent** SIP message.
//!
//! Authoring pattern (mirrors `starter_peer.rs`): a unit struct implementing
//! [`PeerAuditRule`], a `name()` matching the TS `rfc.<x>` rule (`rfc3264.<x>`),
//! an optional narrowed `subject()`, and a `check()` that lenient-parses the
//! sent-direction messages and inspects their SDP body. Structured SDP access
//! reuses [`crate::rfc_audit::offer_answer::parse_sdp_body`] and
//! [`sip_message::sdp::validate_sdp_body`] rather than re-parsing the body.
//! Add the struct to [`peer_rules`].

use std::collections::HashSet;
use std::sync::Arc;

use layer_harness::Stamped;
use sip_message::parser::custom::CustomParser;
use sip_message::sdp::validate_sdp_body;
use sip_message::{SipMessage, SipParser};

use crate::contracts::{PeerAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::call_id;
use crate::rfc_audit::offer_answer::parse_sdp_body;
use crate::types::UaRole;

/// RFC 3264 §5 caps session-version growth to `2^62 - 1`; the TS reference uses
/// the tighter `Number.MAX_SAFE_INTEGER` (`2^53 - 1`) it can observe losslessly
/// in JS. We keep the same observable bound for parity with the reference traces.
const SDP_INTEGER_MAX: i64 = 9_007_199_254_740_991; // 2^53 - 1

/// Iterate the messages this bind **sent** (`SendCalled`), lenient-parsed. SDP
/// the bind mints rides its sent messages, so sent-direction is where an
/// offer/answer body defect is attributed to its originator.
fn sent_messages<'a>(
    events: &'a [Stamped<SignalingNetworkEvent>],
    parser: &'a CustomParser,
) -> impl Iterator<Item = SipMessage> + 'a {
    events.iter().filter_map(move |s| match &s.event {
        SignalingNetworkEvent::SendCalled { msg, .. } => parser.parse(msg).ok(),
        _ => None,
    })
}

/// The raw body bytes of a parsed message (request or response).
fn body_bytes(msg: &SipMessage) -> &[u8] {
    match msg {
        SipMessage::Request(r) => &r.body,
        SipMessage::Response(r) => &r.body,
    }
}

/// True iff a `Content-Type` header value is (case-insensitively) `application/sdp`
/// — the `^application/sdp\b` test in the TS `isSdpBody`. A trailing `;charset=…`
/// or other parameter after the subtype is allowed (the `\b` boundary).
fn is_sdp_content_type(msg: &SipMessage) -> bool {
    for v in msg.get_header("content-type") {
        let t = v.trim();
        if t.len() < 15 {
            // shorter than "application/sdp"
            continue;
        }
        let (head, rest) = t.split_at(15);
        if head.eq_ignore_ascii_case("application/sdp") {
            // `\b` after `sdp`: end of string or a non-word character.
            match rest.chars().next() {
                None => return true,
                Some(c) if !(c.is_ascii_alphanumeric() || c == '_') => return true,
                _ => {}
            }
        }
    }
    false
}

/// True iff a parsed message carries a non-empty SDP body. Mirrors the TS
/// `isSdpBody`: a body present, non-empty, and `Content-Type: application/sdp`.
fn carries_sdp(msg: &SipMessage) -> bool {
    !body_bytes(msg).is_empty() && is_sdp_content_type(msg)
}

/// Lenient SDP-grammar walk for [`SdpBodyParseableRule`]. Returns the first
/// concrete failure label, or `None` when the body satisfies the regression-only
/// checks. Mirrors the TS `checkSdp`:
///   - has `v=0`, `o=`, `s=`, `t=` session-level lines,
///   - exactly one session description (one `v=` line),
///   - `o=`: 6 tokens; sess-id and sess-version are non-negative integers within
///     [`SDP_INTEGER_MAX`],
///   - each `m=` block has a `c=` (session-level or before the next `m=`) and a
///     non-negative-integer port,
///   - every `a=ptime:N` has `N > 0`.
///
/// Empty bodies are skipped upstream. The base RFC 4566 §5 grammar (v=/o=/s=/t=
/// presence, `v=0`, six-token `o=`, m-line arity) is delegated to
/// [`validate_sdp_body`]; this walk layers the extra offer/answer-model bounds.
fn check_sdp(body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(body).into_owned();
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.is_empty())
        .collect();

    // Exactly one session description (one `v=` line), and it must be `v=0`.
    let v_lines: Vec<&&str> = lines.iter().filter(|l| l.starts_with("v=")).collect();
    if v_lines.is_empty() {
        return Some("missing v= line".to_string());
    }
    if v_lines.len() > 1 {
        return Some(format!(
            "{} session descriptions (v= lines) — exactly one required",
            v_lines.len()
        ));
    }
    if *v_lines[0] != "v=0" {
        return Some(format!("unexpected v= value '{}' (expected v=0)", v_lines[0]));
    }

    // Base RFC 4566 §5 grammar (o=/s=/t= presence, six-token o=, m-line arity).
    if let Err(e) = validate_sdp_body(body) {
        return Some(e.reason);
    }

    // o= sess-id / sess-version bounds (M-005 / M-006). validate_sdp_body has
    // already confirmed the six-token o= line exists.
    let o_line = lines.iter().find(|l| l.starts_with("o=")).unwrap();
    let o_tokens: Vec<&str> = o_line[2..].split_whitespace().collect();
    let sess_id = o_tokens[1];
    let sess_version = o_tokens[2];
    if !sess_id.bytes().all(|b| b.is_ascii_digit()) || sess_id.is_empty() {
        return Some(format!("o= sess-id '{sess_id}' is not a non-negative integer"));
    }
    if !sess_version.bytes().all(|b| b.is_ascii_digit()) || sess_version.is_empty() {
        return Some(format!("o= sess-version '{sess_version}' is not a non-negative integer"));
    }
    if sess_id.parse::<i64>().map(|n| n > SDP_INTEGER_MAX).unwrap_or(true) {
        return Some(format!(
            "o= sess-id '{sess_id}' exceeds Number.MAX_SAFE_INTEGER (signed-int64 bound)"
        ));
    }
    if sess_version.parse::<i64>().map(|n| n > SDP_INTEGER_MAX).unwrap_or(true) {
        return Some(format!(
            "o= sess-version '{sess_version}' exceeds Number.MAX_SAFE_INTEGER (signed-int64 bound)"
        ));
    }

    // Walk m= blocks: a session-level c= (before the first m=) satisfies the
    // c=-presence requirement for every block; otherwise each block needs its
    // own c= before the next m= boundary. Each m= line needs an integer port.
    let mut session_level_c = false;
    let mut in_media = false;
    let mut current_media_has_c = false;
    let mut current_media_name = String::new();
    for line in &lines {
        if let Some(m_val) = line.strip_prefix("m=") {
            if in_media && !current_media_has_c && !session_level_c {
                return Some(format!(
                    "m={current_media_name} block has no c= line and no session-level c="
                ));
            }
            let m_tokens: Vec<&str> = m_val.split_whitespace().collect();
            if m_tokens.len() < 3 {
                return Some(format!(
                    "m= line '{line}' has fewer than 3 tokens (expected: media port proto fmt...)"
                ));
            }
            let port_tok = m_tokens[1];
            if port_tok.is_empty() || !port_tok.bytes().all(|b| b.is_ascii_digit()) {
                return Some(format!(
                    "m= line port '{port_tok}' is not a non-negative integer"
                ));
            }
            in_media = true;
            current_media_has_c = false;
            current_media_name = m_tokens[0].to_string();
            continue;
        }
        if !in_media && line.starts_with("c=") {
            session_level_c = true;
        }
        if in_media && line.starts_with("c=") {
            current_media_has_c = true;
        }
        if let Some(raw) = line.strip_prefix("a=ptime:") {
            let raw = raw.trim();
            match raw.parse::<i64>() {
                Ok(n) if n > 0 => {}
                _ => return Some(format!("a=ptime:{raw} is not > 0")),
            }
        }
    }
    if in_media && !current_media_has_c && !session_level_c {
        return Some(format!(
            "m={current_media_name} block has no c= line and no session-level c="
        ));
    }
    None
}

/// **RFC 3264 §5-6 / RFC 4566 — every sent SDP body MUST conform to the
/// offer/answer grammar (RFC3264-MUST-003/-004/-005/-006/-012/-027).** A real
/// UA refuses or 488s a malformed offer/answer (no valid `v=0` session, an
/// over-large `o=` sess-id/version that rolls a signed int64, a non-positive
/// `a=ptime`, a media stream with no `c=` or no port); the test UA accepts the
/// body and silently masks the defect. Checking the **sent** direction attributes
/// a malformed body to the bind that minted it.
pub struct SdpBodyParseableRule;

impl PeerAuditRule for SdpBodyParseableRule {
    fn name(&self) -> &'static str {
        "rfc3264.sdpBodyParseable"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            if !carries_sdp(&msg) {
                continue;
            }
            let Some(failure) = check_sdp(body_bytes(&msg)) else {
                continue;
            };
            out.push(format!(
                "Sent SDP body fails RFC 3264/4566 grammar check: {failure} (callId {}) — \
                 RFC 3264 §5-6 / RFC3264-MUST-003/-004/-005/-006/-012/-027",
                call_id(&msg),
            ));
        }
        out
    }
}

/// True iff a `c=` line value is the unspecified address — `IN IP4 0.0.0.0` or
/// `IN IP6 ::` (the legacy "hold" idiom). Mirrors the TS regexes
/// `/^IN\s+IP4\s+0\.0\.0\.0\b/i` and `/^IN\s+IP6\s+::\b/i`.
fn c_line_is_unspecified(c_val: &str) -> bool {
    let toks: Vec<&str> = c_val.split_whitespace().collect();
    if toks.len() < 3 {
        return false;
    }
    if !toks[0].eq_ignore_ascii_case("IN") {
        return false;
    }
    // IP4 0.0.0.0 — `\b` after the address allows a trailing `/ttl/count`.
    if toks[1].eq_ignore_ascii_case("IP4") {
        let addr = toks[2];
        if addr == "0.0.0.0"
            || addr
                .strip_prefix("0.0.0.0")
                .map(|r| r.starts_with('/'))
                .unwrap_or(false)
        {
            return true;
        }
    }
    // IP6 :: — `\b` after `::` allows a trailing `/count`.
    if toks[1].eq_ignore_ascii_case("IP6") {
        let addr = toks[2];
        if addr == "::" || addr.strip_prefix("::").map(|r| r.starts_with('/')).unwrap_or(false) {
            return true;
        }
    }
    false
}

/// **RFC 3264 §6 / §8.4 — a held stream `c=0.0.0.0` MUST NOT also carry port 0
/// (RFC3264-MUST-051).** The legacy "hold" idiom sets the connection address to
/// the unspecified address; combining it with `m=… 0 …` (port 0, the §6
/// "rejected stream" marker) produces an ambiguous "held AND rejected" state a
/// real peer cannot disambiguate. Subject is the UAC only — the TS rule narrows
/// to `{uac}` because the answerer/B2BUA legitimately emits `m=… 0` to reject a
/// stream while echoing a real (non-unspecified) `c=`.
pub struct C0PortNonZeroRule;

impl PeerAuditRule for C0PortNonZeroRule {
    fn name(&self) -> &'static str {
        "rfc3264.c0PortNonZero"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Uac])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            let body = body_bytes(&msg);
            if body.is_empty() {
                continue;
            }
            // Reuse the shared SDP shape: session-level c= + per-block c=/port.
            let Some(doc) = parse_sdp_body(body) else {
                continue;
            };
            let session_unspecified =
                doc.c_line.as_deref().map(c_line_is_unspecified).unwrap_or(false);
            for media in &doc.media {
                let media_unspecified = media.c_line.as_deref().map(c_line_is_unspecified);
                let applicable = match media_unspecified {
                    Some(u) => u,
                    None => session_unspecified,
                };
                if applicable && media.port == Some(0) {
                    out.push(format!(
                        "SDP body has c=0.0.0.0 and m={} port=0 simultaneously (callId {}) — \
                         RFC 3264 §6 / RFC3264-MUST-051",
                        media.r#type,
                        call_id(&msg),
                    ));
                }
            }
        }
        out
    }
}

/// The peer rules defined in this module. Aggregated by [`super::rfc_peer_rules`].
pub(crate) fn peer_rules() -> Vec<Arc<dyn PeerAuditRule>> {
    vec![Arc::new(SdpBodyParseableRule), Arc::new(C0PortNonZeroRule)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    /// Build a sent INVITE with the given SDP body (application/sdp).
    fn invite_with_sdp(body: &str) -> Vec<u8> {
        let body_bytes = body.as_bytes();
        format!(
            "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-abc\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body_bytes.len()
        )
        .into_bytes()
    }

    /// An INVITE with a body but a non-SDP Content-Type (must be skipped).
    fn invite_with_text_body(body: &str) -> Vec<u8> {
        format!(
            "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-abc\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn sent_at(bind: &str, raw: Vec<u8>, to: &str, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: bind.to_string(),
                to: to.parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    fn recv_at(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                packet: UdpPacket {
                    raw,
                    src: "127.0.0.1:9999".parse().unwrap(),
                    arrival_ms: seq,
                },
            },
            seq,
            at_ms: seq,
        }
    }

    const GOOD_SDP: &str = "v=0\r\n\
o=alice 2890844526 2890844526 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=ptime:20\r\n";

    // ----- sdpBodyParseable -----

    #[test]
    fn well_formed_sdp_is_clean() {
        let evs = vec![sent_at("alice", invite_with_sdp(GOOD_SDP), "127.0.0.1:5070", 0)];
        assert!(SdpBodyParseableRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn two_session_descriptions_are_flagged() {
        // Two v= lines → "2 session descriptions".
        let bad = "v=0\r\nv=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n";
        let evs = vec![sent_at("alice", invite_with_sdp(bad), "127.0.0.1:5070", 0)];
        let f = SdpBodyParseableRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("session descriptions"), "{}", f[0]);
    }

    #[test]
    fn nonpositive_ptime_is_flagged() {
        let bad = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\na=ptime:0\r\n";
        let evs = vec![sent_at("alice", invite_with_sdp(bad), "127.0.0.1:5070", 0)];
        let f = SdpBodyParseableRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("ptime"), "{}", f[0]);
    }

    #[test]
    fn media_block_without_c_is_flagged() {
        // No session-level c= and the audio block has none either.
        let bad = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        let evs = vec![sent_at("alice", invite_with_sdp(bad), "127.0.0.1:5070", 0)];
        let f = SdpBodyParseableRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("no c= line"), "{}", f[0]);
    }

    #[test]
    fn non_sdp_content_type_is_skipped() {
        // A malformed-looking text body must not be SDP-checked.
        let evs =
            vec![sent_at("alice", invite_with_text_body("not sdp at all"), "127.0.0.1:5070", 0)];
        assert!(SdpBodyParseableRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn received_sdp_is_not_judged_by_sender_rule() {
        let evs = vec![recv_at("alice", invite_with_sdp("v=0\r\nm=audio 1 RTP/AVP 0\r\n"), 0)];
        assert!(SdpBodyParseableRule.check(&evs, "alice").is_empty());
    }

    // ----- c0PortNonZero -----

    #[test]
    fn held_stream_with_real_port_is_clean() {
        // c=0.0.0.0 (hold) but a real port → not the ambiguous combination.
        let body = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n";
        let evs = vec![sent_at("alice", invite_with_sdp(body), "127.0.0.1:5070", 0)];
        assert!(C0PortNonZeroRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn rejected_stream_with_real_address_is_clean() {
        // port 0 (reject) but a real c= address → not the ambiguous combination.
        let body = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
m=audio 0 RTP/AVP 0\r\n";
        let evs = vec![sent_at("alice", invite_with_sdp(body), "127.0.0.1:5070", 0)];
        assert!(C0PortNonZeroRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn session_c0_and_port0_is_flagged() {
        let body = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
m=audio 0 RTP/AVP 0\r\n";
        let evs = vec![sent_at("alice", invite_with_sdp(body), "127.0.0.1:5070", 0)];
        let f = C0PortNonZeroRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST-051"), "{}", f[0]);
    }

    #[test]
    fn media_level_c0_and_port0_is_flagged() {
        // Session c= is a real address; the audio block overrides with 0.0.0.0.
        let body = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
m=audio 0 RTP/AVP 0\r\nc=IN IP4 0.0.0.0\r\n";
        let evs = vec![sent_at("alice", invite_with_sdp(body), "127.0.0.1:5070", 0)];
        let f = C0PortNonZeroRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("m=audio port=0"), "{}", f[0]);
    }

    #[test]
    fn media_c_override_masks_session_c0() {
        // Session-level 0.0.0.0 but the (port-0) block overrides with a real c=
        // → the applicable c= is not unspecified, so no finding.
        let body = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
m=audio 0 RTP/AVP 0\r\nc=IN IP4 10.0.0.1\r\n";
        let evs = vec![sent_at("alice", invite_with_sdp(body), "127.0.0.1:5070", 0)];
        assert!(C0PortNonZeroRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn c0_subject_is_uac_only() {
        assert_eq!(C0PortNonZeroRule.subject(), HashSet::from([UaRole::Uac]));
    }
}

//! The valid forms and refusals extension RFCs state.

use super::*;

/// The valid forms and refusals extension RFCs state.
pub(super) fn rows() -> Vec<Row> {
    let head = options();
    vec![
        // RFC 3261 §22.4, ADR-0001: credentials a reader takes whole.
        answered(
            "3261-22.4-credentials-no-comma",
            head.clone().add("Authorization: Digest realm=\"a\" nonce=\"b\""),
        ),
        answered(
            "3261-22.4-credentials-unquoted-space",
            head.clone().add("Authorization: Digest realm=a b"),
        ),
        answered(
            "3261-22.4-credentials-comment",
            head.clone().add("Authorization: Digest realm=(x)"),
        ),
        answered(
            "3261-22.4-credentials-empty-value",
            head.clone().add("Authorization: Digest realm="),
        ),
        // RFC 3261 §25.1: an option tag is a token, and ingress reads every
        // option-tag line: a request is answered 400, a response dropped.
        answered("3261-25.1-option-tag-space", head.clone().add("Proxy-Require: foo bar")),
        answered("3261-25.1-option-tag-quoted", head.clone().add("Proxy-Require: \"foo\"")),
        answered("3261-25.1-option-tag-slash", head.clone().add("Require: 100rel, foo/bar")),
        answered("3261-25.1-option-tag-empty-entry", head.clone().add("Require: 100rel,,timer")),
        answered("3261-25.1-option-tag-empty-require", head.clone().add("Require:")),
        answered("3261-25.1-option-tag-supported-space", head.clone().add("Supported: timer, x y")),
        accept("3261-20.37-supported-empty", head.clone().add("Supported:")),
        dropped(
            "3262-3-reliable-provisional-option-tag-space",
            response(183, "INVITE").with("rseq", "RSeq: 1").add("Require: 100rel, x y"),
        ),
        dropped(
            "3261-25.1-option-tag-supported-in-a-response",
            response(200, "OPTIONS").add("Supported: timer, x y"),
        ),
        // RFC 6665 §8.2.1: `Event = event-type *( SEMI event-param )`.
        answered("6665-8.2.1-event-not-a-token", request("SUBSCRIBE").with("event", "Event: a b")),
        answered("6665-8.2.1-event-empty", request("SUBSCRIBE").with("event", "Event:")),
        answered(
            "6665-8.2.1-event-parameter-not-a-token",
            request("SUBSCRIBE").with("event", "Event: refer;id=a b"),
        ),
        // RFC 3262 §7: the response number.
        accept("3262-7.1-rseq-lowest", response(183, "INVITE").with("rseq", "RSeq: 1")),
        accept("3262-7.1-rseq-highest", response(183, "INVITE").with("rseq", "RSeq: 4294967295")),
        dropped("3262-7.1-rseq-zero", response(183, "INVITE").with("rseq", "RSeq: 0")),
        dropped("3262-7.1-rseq-not-digits", response(183, "INVITE").with("rseq", "RSeq: abc")),
        dropped(
            "3262-7.1-rseq-past-the-range",
            response(183, "INVITE").with("rseq", "RSeq: 4294967296"),
        ),
        // RFC 3262 §7.2: RAck's numbers have RSeq's and CSeq's ranges.
        accept("3262-7.2-rack-highest", in_dialog("PRACK", "RAck: 4294967295 4294967295 INVITE")),
        // RFC 3323 §4.2: `;` separates; a comma does not.
        // RFC 3323 §4.2 separates values with `;`, its erratum 5184 (Verified)
        // with `,`: both read, and so does a line holding both.
        holds(
            "3323-4.2-privacy-semicolons",
            head.clone().add("Privacy: id;user;critical"),
            privacy_reads_three,
        ),
        holds(
            "3323-4.2-privacy-commas",
            head.clone().add("Privacy: id, user, critical"),
            privacy_reads_three,
        ),
        holds(
            "3323-4.2-privacy-both-separators",
            head.clone().add("Privacy: id;user, critical"),
            privacy_reads_three,
        ),
        // RFC 3325 §9.1: no header parameters, and a bare URI keeps its own.
        unread(
            "3325-9.1-pai-header-parameter",
            head.clone().add("P-Asserted-Identity: <sip:a@example.com>;foo=bar"),
            typed::<header::PAssertedIdentity>,
        ),
        holds(
            "3325-9.1-bare-tel-keeps-its-parameter",
            head.clone().add("P-Asserted-Identity: tel:+15551234;phone-context=x.example"),
            |msg| {
                let values = msg.list::<header::PAssertedIdentity>().map_err(|e| e.reason)?;
                let uri = values.first().ok_or("no value")?.uri().text().into_owned();
                uri.contains("phone-context").then_some(()).ok_or(format!("the URI is {uri}"))
            },
        ),
        // RFC 3326: not judged at ingress, and a cause reads.
        holds(
            "rule-reason-off-grammar-parameter",
            head.clone().add("Reason: Q.850;cause=16;host=<sip:a.example.com>"),
            |msg| {
                let reason =
                    msg.header::<header::Reason>().ok_or("no value")?.map_err(|e| e.reason)?;
                reason
                    .params()
                    .value("cause")
                    .filter(|c| *c == "16")
                    .map(|_| ())
                    .ok_or("no cause".to_string())
            },
        ),
        accept(
            "rule-charging-vector-unquoted-at",
            head.clone().add("P-Charging-Vector: icid-value=abc@example.com"),
        ),
        // RFC 3891 §6.1: exactly one to-tag and one from-tag, after one callid.
        unread(
            "3891-6.1-replaces-no-tags",
            request("INVITE").add("Replaces: abc@example.com"),
            typed::<header::Replaces>,
        ),
        unread(
            "3891-6.1-replaces-call-id-with-space",
            request("INVITE").add("Replaces: a b;to-tag=1;from-tag=2"),
            typed::<header::Replaces>,
        ),
        // RFC 4028: the session-timer tail.
        holds(
            "4028-5-min-se-parameter",
            request("INVITE").add("Min-SE: 90;foo=bar"),
            typed::<header::MinSe>,
        ),
        answered("4028-5-min-se-junk-parameters", request("INVITE").add("Min-SE: 90;=;;")),
        answered(
            "4028-4-session-expires-junk-parameters",
            request("INVITE").add("Session-Expires: 90;=;;"),
        ),
        // RFC 3261 §25.1: Retry-After's comment.
        holds(
            "3261-20.33-retry-after-comment-then-parameter",
            response(503, "OPTIONS").add("Retry-After: 120 (back; soon);duration=60"),
            |msg| {
                let retry =
                    msg.header::<header::RetryAfter>().ok_or("no value")?.map_err(|e| e.reason)?;
                retry
                    .params()
                    .value("duration")
                    .filter(|d| *d == "60")
                    .map(|_| ())
                    .ok_or(format!("read {}", sip_message::header::HeaderValue::to_wire(&retry)))
            },
        ),
        unread(
            "3261-20.33-retry-after-not-digits",
            response(503, "OPTIONS").add("Retry-After: abc"),
            typed::<header::RetryAfter>,
        ),
        // RFC 4488 §3, RFC 6665 §8.4: one leading token.
        unread(
            "4488-3-refer-sub-other-value",
            request("REFER").add("Refer-Sub: maybe x;y"),
            typed::<header::ReferSub>,
        ),
        unread(
            "6665-8.4-event-two-types",
            request("SUBSCRIBE").with("event", "Event: a, b"),
            typed::<header::Event>,
        ),
        unread(
            "6665-8.4-event-space-in-the-type",
            request("SUBSCRIBE").with("event", "Event: foo bar;id=1"),
            typed::<header::Event>,
        ),
        unread(
            "6665-8.4-allow-events-space-in-a-type",
            head.clone().add("Allow-Events: a b"),
            typed::<header::AllowEvents>,
        ),
    ]
}

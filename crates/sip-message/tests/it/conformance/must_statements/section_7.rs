//! RFC 3261 §7: the message's shape and its header fields.

use super::*;

/// RFC 3261 §7: the message's shape and its header fields.
pub(super) fn rows() -> Vec<Row> {
    let mut rows = vec![
        // §7.1: SIP-Version is compared case-insensitively.
        accept("3261-7.1-version-lower-request", options().start_line("OPTIONS sip:bob@example.com sip/2.0")),
        accept("3261-7.1-version-mixed-request", options().start_line("OPTIONS sip:bob@example.com Sip/2.0")),
        accept("3261-7.1-version-lower-response", response(200, "OPTIONS").start_line("sip/2.0 200 OK")),
        Row::new("must/3261-7.1-sipfrag-version-lower", match sipfrag::status(b"sip/2.0 200 OK\r\n") {
            Some(200) => Ok(()),
            other => Err(format!("status {other:?}")),
        }),
        // ADR-0054 decides the outcome over RFC 4475's SHOULD of a 505: a refused
        // start line leaves no partial.
        dropped_because("4475-3.1.2.16-version-unknown", "version", options().start_line("OPTIONS sip:bob@example.com SIP/7.0")),
        // ADR-0001, status codes: §7.2 gives six values for the first digit.
        accept("3261-7.2-status-lowest", response(100, "OPTIONS").start_line("SIP/2.0 100 Trying")),
        accept("3261-7.2-status-highest", response(699, "OPTIONS").start_line("SIP/2.0 699 Reason")),
        dropped_because("adr-0001-status-below-100", "status", response(200, "OPTIONS").start_line("SIP/2.0 099 Reason")),
        dropped_because("adr-0001-status-above-699", "status", response(200, "OPTIONS").start_line("SIP/2.0 700 Reason")),
        // §7.3.1: the header field format.
        accept("3261-7.3.1-space-before-colon", options().add("Subject : a")),
        accept("3261-7.3.1-tab-before-colon", options().add("Subject\t: a")),
        accept("3261-7.3.1-no-space-after-colon", options().add("Subject:a")),
        accept("3261-7.3.1-fold-after-colon", options().add("Subject:\r\n a")),
        accept("3261-7.3.1-name-any-case", options().add("sUbJeCt: a")),
        // §7.3.1's prose allows whitespace before the colon; HCOLON admits only
        // SP and HTAB there, so a fold before it is refused.
        dropped_because("3261-7.3.1-fold-before-colon", "0x3a", options().with("from", "From\r\n : <sip:alice@example.com>;tag=a1")),
        accept("3261-7.3.1-route-on-two-lines", options().add("Route: <sip:a.example.com;lr>").add("Route: <sip:b.example.com;lr>")),
        accept("3261-7.3.1-route-on-one-line", options().add("Route: <sip:a.example.com;lr>, <sip:b.example.com;lr>")),
        accept("3261-7.3.1-via-on-one-line", options().with("via", "Via: SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a, SIP/2.0/UDP 192.0.2.11;branch=z9hG4bK-b")),
        accept("3261-20.14-trailing-space-content-length", options().unframed().add("Content-Length: 0 ")),
        accept("3261-25.1-trailing-space-via", options().with("via", "Via: SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a  ")),
        accept("3261-25.1-trailing-space-from", options().with("from", "From: <sip:alice@example.com>;tag=a1\t")),
        accept("3261-25.1-trailing-space-call-id", options().with("call-id", "Call-ID: abc@example.com ")),
        accept("3261-25.1-trailing-space-cseq", options().with("cseq", "CSeq: 1 OPTIONS ")),
        accept("3261-25.1-trailing-fold", options().with("from", "From: <sip:alice@example.com>;tag=a1\r\n ")),
        // Off-grammar since erratum 7529 (no whitespace before a fold's CRLF), yet
        // it cannot change the meaning, the test for a refusal: it reads.
        accept("3261-25.1-space-before-a-fold-before-a-parameter", options().with("from", "From: <sip:alice@example.com> \r\n ;tag=a1")),
        // §7.3.3: every compact form of §20 and the extension RFCs.
    ];
    rows.extend([
        // §7.3.1: the credentials and challenges take several lines, unlisted.
        accept(
            "3261-7.3.1-two-authorization-lines",
            options()
                .add(credentials("Authorization", "r1"))
                .add(credentials("Authorization", "r2")),
        ),
        accept(
            "3261-7.3.1-two-proxy-authorization-lines",
            options()
                .add(credentials("Proxy-Authorization", "r1"))
                .add(credentials("Proxy-Authorization", "r2")),
        ),
        accept(
            "3261-7.3.1-two-www-authenticate-lines",
            response(401, "OPTIONS")
                .add("WWW-Authenticate: Digest realm=\"a\", nonce=\"b\"")
                .add("WWW-Authenticate: Digest realm=\"c\", nonce=\"d\""),
        ),
    ]);
    let compact: [(&str, &str, ReadCheck); 4] = [
        ("o", "refer", typed::<header::Event>),
        ("u", "refer", typed::<header::AllowEvents>),
        ("r", "<sip:carol@example.com>", typed::<header::ReferTo>),
        ("b", "<sip:carol@example.com>", typed::<header::ReferredBy>),
    ];
    for (letter, value, read) in compact {
        rows.push(holds(
            &format!("3261-7.3.3-compact-{letter}"),
            options().add(format!("{letter}: {value}")),
            read,
        ));
    }
    rows.extend([
        // §7.3.1: a value the grammar lets be empty, and one it does not.
        accept("3261-25.1-empty-supported", options().add("Supported:")),
        accept("3261-25.1-empty-allow", options().add("Allow:")),
        accept("3261-25.1-empty-accept-encoding", options().add("Accept-Encoding:")),
        accept("3261-25.1-empty-accept", options().add("Accept:")),
        accept("3261-25.1-empty-subject", options().add("Subject:")),
        accept("5009-9-empty-p-early-media", options().add("P-Early-Media:")),
        unread("3261-25.1-empty-require", options().add("Require:"), typed::<header::Require>),
        unread(
            "3261-25.1-empty-proxy-require",
            options().add("Proxy-Require:"),
            typed::<header::ProxyRequire>,
        ),
        unread(
            "3261-25.1-empty-unsupported",
            options().add("Unsupported:"),
            typed::<header::Unsupported>,
        ),
        unread(
            "6665-8.4-empty-allow-events",
            options().add("Allow-Events:"),
            typed::<header::AllowEvents>,
        ),
        dropped("3261-25.1-empty-via-beside-a-via", options().add("Via:")),
        answered("3261-25.1-empty-contact", request("REGISTER").with("contact", "Contact:")),
        answered("3261-25.1-empty-route", options().add("Route:")),
        answered("3261-25.1-empty-record-route", options().add("Record-Route:")),
        unread(
            "3325-9.1-empty-p-asserted-identity",
            options().add("P-Asserted-Identity:"),
            typed::<header::PAssertedIdentity>,
        ),
        answered("3261-25.1-empty-content-type", options().add("Content-Type:")),
        unread(
            "3261-25.1-empty-content-disposition",
            options().add("Content-Disposition:"),
            typed::<header::ContentDisposition>,
        ),
        // A header the stack does not know may repeat, and so may a list header.
        accept("rule-repeat-extension-header", options().add("X-Note: a").add("X-Note: b")),
        accept(
            "rule-repeat-p-asserted-identity",
            options()
                .add("P-Asserted-Identity: <sip:a@example.com>")
                .add("P-Asserted-Identity: <tel:+15551234>"),
        ),
        accept(
            "rule-repeat-call-info",
            options()
                .add("Call-Info: <http://example.com/a>")
                .add("Call-Info: <http://example.com/b>"),
        ),
        // §25.1: a word may hold a DQUOTE that opens nothing.
        accept(
            "3261-25.1-word-with-a-dquote-then-a-control-byte",
            request("INVITE").add(b"Replaces: a\"b;to-tag=1;from-tag=2;x=\"\\\x04\"".to_vec()),
        ),
        // §25.1: an NBSP is content of TEXT-UTF8-TRIM, at an edge or inside.
        accept("3261-25.1-nbsp-in-a-subject", options().add("Subject: a\u{a0}b")),
        accept("3261-25.1-nbsp-at-a-subject-edge", options().add("Subject: \u{a0}a\u{a0}")),
        // §25.1: TEXT-UTF8-TRIM has no quoted-string.
        accept(
            "3261-25.1-subject-unbalanced-quote",
            options().add("Subject: The caller said \"hello"),
        ),
    ]);
    rows
}

//! RFC 3261 §18.3, §19.1.1, §20.14, §20.15 and §25.1, and ADR-0001's bound.

use super::*;

/// RFC 3261 §18.3, §19.1.1, §20.14, §20.15 and §25.1: framing, URIs and bytes.
pub(super) fn rows() -> Vec<Row> {
    let head = options();
    vec![
        // §18.3, §20.14: what frames the body.
        answered(
            "3261-18.3-content-length-past-the-datagram",
            head.clone().unframed().add("Content-Length: 5").body("abc"),
        ),
        accept(
            "3261-18.3-content-length-short-of-the-datagram",
            head.clone()
                .unframed()
                .add("Content-Type: text/plain")
                .add("Content-Length: 1")
                .body("abc"),
        ),
        answered(
            "4475-3.1.2.3-content-length-negative",
            head.clone().unframed().add("Content-Length: -1"),
        ),
        accept("3261-18.3-no-content-length", head.clone().unframed()),
        // §19.1.1 Table 1, ADR-0053 §2: a Request-URI's form.
        answered_because(
            "adr-0053-2-request-uri-headers",
            "Request-URI",
            head.clone().start_line("OPTIONS sip:bob@example.com?Subject=a SIP/2.0"),
        ),
        answered_because(
            "adr-0053-2-request-uri-empty-host",
            "Request-URI",
            head.clone().start_line("OPTIONS sip:bob@ SIP/2.0"),
        ),
        // §19.1.1: a URI parameter name appears once; no exemption names a
        // Request-URI.
        answered_because(
            "3261-19.1.1-request-uri-parameter-twice-same",
            "written twice",
            head.clone().start_line("OPTIONS sip:bob@example.com;x=1;x=1 SIP/2.0"),
        ),
        answered_because(
            "3261-19.1.1-request-uri-parameter-twice-recased",
            "written twice",
            head.clone()
                .start_line("OPTIONS sip:bob@example.com;transport=udp;Transport=udp SIP/2.0"),
        ),
        answered_because(
            "3261-19.1.1-request-uri-parameter-twice-different",
            "Request-URI",
            head.clone().start_line("OPTIONS sip:bob@example.com;x=1;x=2 SIP/2.0"),
        ),
        answered("rule-route-opaque-scheme", head.clone().add("Route: <urn:service:sos>")),
        // ADR-0053 §2: `transport`, `user` and `method` take a token, or a
        // `paramchar` run as the `other-param` every parameter also is.
        accept(
            "adr-0053-2-user-parameter-paramchar",
            options().with("from", "From: <sip:alice@example.com;user=a:b>;tag=a1"),
        ),
        accept(
            "adr-0053-2-transport-parameter-paramchar",
            options().with("from", "From: <sip:alice@example.com;transport=x/y>;tag=a1"),
        ),
        accept(
            "adr-0053-2-method-parameter-token",
            options().with("from", "From: <sip:alice@example.com;method=x`y>;tag=a1"),
        ),
        answered(
            "adr-0053-2-user-parameter-neither",
            request("INVITE").with("contact", "Contact: <sip:alice@192.0.2.10;user=a\"b>"),
        ),
        // ADR-0053 §2: a URI parameter outside the `transport`, `user` and
        // `method` of §25.1 is an `other-param`, held to `paramchar`.
        answered(
            "adr-0053-2-other-param-outside-paramchar",
            request("INVITE").with("contact", "Contact: <sip:alice@192.0.2.10;x=a`b>"),
        ),
        // §10.2.1.1: a Contact `expires` past 2^32-1 reads, as 2^32-1 at most.
        accept(
            "3261-10.2.1.1-contact-expires-past-2-32",
            request("REGISTER")
                .with("contact", "Contact: <sip:alice@192.0.2.10>;expires=4294967296"),
        ),
        // RFC 3323 erratum 5184: a Privacy the stack writes separates with `,`.
        Row::new("must/3323-4.2-privacy-written-with-commas", privacy_written_with_commas()),
        // ADR-0053 §2: a tel URI's number stands as its host, so a route entry
        // of one reads.
        accept("adr-0053-2-route-tel", head.clone().add("Route: <tel:+15551234;lr>")),
        answered_because(
            "adr-0053-2-record-route-headers",
            "escaped headers",
            head.clone().add("Record-Route: <sip:a.example.com;lr?Subject=x>"),
        ),
        // RFC 5954 §4.1: an IPv6 address holds at most eight groups.
        dropped(
            "5954-4.1-ipv6-of-nine-groups",
            head.clone().with("to", "To: <sip:bob@[1:2:3:4:5:6:7:8:9]>"),
        ),
        // §25.1: bytes of a value.
        dropped_because("3261-25.1-bare-cr", "bare CR", head.clone().add("Subject: a\rb")),
        dropped_because("3261-25.1-bare-lf", "bare LF", head.clone().add("Subject: a\nb")),
        answered("3261-25.1-nul-in-value", head.clone().add("Subject: a\0b")),
        answered("3261-25.1-del-in-value", head.clone().add("Subject: a\x7fb")),
        dropped("adr-0054-2-non-utf8-in-a-name", head.clone().add(b"Vi\xffa: x".to_vec())),
        accept(
            "4475-3.1.1.2-quoted-pair-control",
            head.clone().with("to", b"To: \"BEL:\\\x07\" <sip:bob@example.com>".to_vec()),
        ),
        answered(
            "3261-25.1-quoted-pair-where-no-quoted-string",
            head.clone().add(b"Organization: \"a\\\0b\"".to_vec()),
        ),
        answered(
            "3261-25.1-quoted-pair-of-non-ascii",
            head.clone().add("Reply-To: \"a\\\u{e9}\" <sip:a@example.com>"),
        ),
        answered(
            "3261-25.1-quoted-pair-of-a-fold",
            head.clone().add("Reply-To: \"a\\\r\n b\" <sip:a@example.com>"),
        ),
        unread(
            "3261-25.1-unterminated-quote-disposition",
            head.clone().add("Content-Disposition: session;handling=\"x"),
            typed::<header::ContentDisposition>,
        ),
        dropped(
            "3261-25.1-unterminated-quote-via",
            head.clone().with("lower-via", "Via: SIP/2.0/UDP 192.0.2.11;branch=z9hG4bK-b;x=\"abc"),
        ),
        answered(
            "3261-25.1-unterminated-quote-content-type",
            head.clone().add("Content-Type: multipart/mixed;boundary=\"abc"),
        ),
        unread(
            "3261-25.1-unterminated-quote-event",
            head.clone().add("Event: a;x=\"b, c"),
            typed::<header::Event>,
        ),
        answered(
            "4028-4-unterminated-quote-session-expires",
            head.clone().add("Session-Expires: 90;x=\"abc"),
        ),
        // ADR-0053 §3: what the header block's bytes may be. A well-formed surrogate
        // pair is ordinary non-ASCII where the grammar allows it.
        accept(
            "3261-25.1-utf8-display-name",
            head.clone().with("from", "From: \"caf\u{e9}\" <sip:alice@example.com>;tag=a1"),
        ),
        accept_raw(
            "rule-cesu8-pair-in-a-display-name",
            &head
                .clone()
                .with(
                    "from",
                    b"From: \"\xed\xa0\xbc\xed\xbc\xb9\" <sip:alice@example.com>;tag=a1".to_vec(),
                )
                .bytes(),
        ),
        answered_because(
            "adr-0053-3-latin1-byte",
            "UTF-8",
            head.clone().add(b"Subject: caf\xe9".to_vec()),
        ),
        answered_because(
            "adr-0053-3-lone-surrogate-half",
            "UTF-8",
            head.clone().add(b"Subject: \xed\xa0\xbc".to_vec()),
        ),
        answered_because(
            "adr-0053-3-overlong-form",
            "UTF-8",
            head.clone().add(b"Subject: \xc0\xaf".to_vec()),
        ),
        answered_because(
            "adr-0053-3-five-byte-form",
            "UTF-8",
            head.clone().add(b"Subject: \xf8\x88\x80\x80\x80".to_vec()),
        ),
        dropped_because(
            "rule-cesu8-pair-in-a-tag",
            "UTF-8",
            head.clone().with(
                "from",
                b"From: <sip:alice@example.com>;tag=\xed\xa0\xbc\xed\xbc\xb9".to_vec(),
            ),
        ),
        dropped_because(
            "rule-cesu8-pair-in-a-call-id",
            "UTF-8",
            head.clone().with("call-id", b"Call-ID: \xed\xa0\xbc\xed\xbc\xb9".to_vec()),
        ),
    ]
}

/// ADR-0001's first-ingress bound: a party's new request at its first hop.
pub(super) fn bounds() -> Vec<Row> {
    let line = |length: usize| format!("Subject: {}", "x".repeat(length - "Subject: ".len()));
    let with_lines = |count: usize| (0..count).fold(options(), |t, _| t.add("Subject: a"));
    // The template's own lines, the start line and its Content-Length among them.
    let own = options().bytes().iter().filter(|b| **b == b'\n').count() - 1;
    let uri = |length: usize| {
        let host = "x".repeat(length - "sip:bob@".len());
        format!("OPTIONS sip:bob@{host} SIP/2.0")
    };
    vec![
        accept("adr-0001-line-at-the-bound", options().add(line(8192))),
        answered("adr-0001-line-past-the-bound", options().add(line(8193))),
        accept("adr-0001-request-uri-at-the-bound", options().start_line(uri(8192))),
        answered_because(
            "adr-0001-request-uri-past-the-bound",
            "Request-URI",
            options().start_line(uri(8193)),
        ),
        accept("adr-0001-block-at-the-bound", with_lines(100 - own)),
        answered_because("adr-0001-block-past-the-bound", "header block", with_lines(101 - own)),
    ]
}

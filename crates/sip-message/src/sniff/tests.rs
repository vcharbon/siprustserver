use super::*;

#[test]
fn line_at_names_the_row_spanning_an_offset() {
    let raw =
        b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP h;branch=z9hG4bK1\r\n\r\nv=0\r\no=- 1 1 IN IP4 h\r\n";
    assert_eq!(line_at(raw, 0).as_deref(), Some("SIP/2.0 200 OK"));
    let via_at = raw.iter().position(|&b| b == b'V').unwrap();
    assert_eq!(
        line_at(raw, via_at + 10).as_deref(),
        Some("Via: SIP/2.0/UDP h;branch=z9hG4bK1"),
        "an offset inside a row names the whole row"
    );
    let body_at = raw.len() - 3;
    assert_eq!(
        line_at(raw, body_at).as_deref(),
        Some("o=- 1 1 IN IP4 h"),
        "body lines read the same way"
    );
    assert_eq!(line_at(raw, raw.len()), None, "past the end names nothing");
}

#[test]
fn via_branches_read_the_whole_stack_top_first() {
    let raw = b"BYE sip:x SIP/2.0\r\n\
            Via: SIP/2.0/UDP p;branch=z9hG4bK-p, SIP/2.0/UDP q;received=1.2.3.4\r\n\
            v: SIP/2.0/UDP a;branch=z9hG4bK-a;rport\r\n\
            CSeq: 2 BYE\r\n\r\n";
    assert_eq!(via_branches(raw), vec!["z9hG4bK-p", "", "z9hG4bK-a"]);
}

#[test]
fn from_tag_extracts_the_from_parameter() {
    let raw = b"BYE sip:b@h SIP/2.0\r\nf: <sip:a@h>;tag=a1\r\nTo: <sip:b@h>;tag=b1\r\n\r\n";
    assert_eq!(from_tag(raw), "a1");
    assert_eq!(from_tag(b"BYE sip:b@h SIP/2.0\r\nFrom: <sip:a@h>\r\n\r\n"), "");
}

#[test]
fn to_tag_extracts_the_to_parameter() {
    assert_eq!(to_tag(b"SIP/2.0 180 X\r\nTo: <sip:b@h>;tag=abc\r\n\r\n"), "abc");
    assert_eq!(
        to_tag(b"SIP/2.0 100 Trying\r\nTo: <sip:b@h>\r\n\r\n"),
        "",
        "a tagless To yields empty"
    );
    assert_eq!(
        to_tag(b"SIP/2.0 200 OK\r\nt: <sip:b@h>;tag=Z9\r\n\r\n"),
        "Z9",
        "the compact To form is parsed"
    );
}

#[test]
fn cseq_scanners_share_one_line_walk() {
    let raw = b"BYE sip:x SIP/2.0\r\nCSeq:  7   BYE\r\n\r\n";
    assert_eq!(cseq_number(raw), Some(7));
    assert_eq!(cseq_value(raw), "CSeq: 7   BYE");
    assert_eq!(cseq_method_label(raw), "BYE");
    assert_eq!(cseq_method_label(b"OPTIONS sip:x SIP/2.0\r\n\r\n"), "none");
    assert_eq!(cseq_method_label(b"X sip:x SIP/2.0\r\nCSeq: 1 WEIRD\r\n\r\n"), "other");
}

#[test]
fn option_tags_union_rows_and_folds_into_one_set() {
    let raw = b"INVITE sip:b@h SIP/2.0\r\n\
                    Supported: 100rel, timer\r\n\
                    k: 100REL,replaces\r\n\r\n";
    assert_eq!(
        option_tags(raw, "Supported"),
        ["100rel", "timer", "replaces"],
        "rows and comma folds are one set, the compact spelling included, \
             and a repeat of a tag already in it adds nothing"
    );
    assert_eq!(option_tags(raw, "Require"), Vec::<String>::new(), "absent is empty");
}

#[test]
fn option_tags_drops_a_value_that_is_not_a_token() {
    let raw = b"INVITE sip:b@h SIP/2.0\r\nRequire: two words, timer,\r\n\r\n";
    assert_eq!(option_tags(raw, "Require"), ["timer"]);
}

/// RFC 3323 §4.2 gives `Privacy` a `;`-separated list; the reader follows
/// the header's own grammar rather than assuming a comma.
#[test]
fn option_tags_follow_the_headers_own_separator() {
    let raw = b"INVITE sip:b@h SIP/2.0\r\nPrivacy: id;user\r\n\r\n";
    assert_eq!(option_tags(raw, "Privacy"), ["id", "user"]);
}

#[test]
fn request_uri_reads_the_second_request_line_token() {
    assert_eq!(
        request_uri(b"INVITE sip:bob@10.0.0.1:5070 SIP/2.0\r\n\r\n").as_deref(),
        Some("sip:bob@10.0.0.1:5070")
    );
    assert_eq!(request_uri(b"SIP/2.0 200 OK\r\n\r\n"), None, "a response has no Request-URI");
}

/// Assemble a request with the given request-line method token and extra
/// header lines (fixture only — the classifier under test never allocates).
fn req(method: &str, headers: &str) -> Vec<u8> {
    format!(
        "{method} sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-shed\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
{headers}Call-ID: shed@10.0.0.1\r\n\
CSeq: 1 {method}\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[test]
fn new_invite_is_sheddable() {
    assert!(is_sheddable_new_invite(&req("INVITE", "To: <sip:bob@example.com>\r\n")));
}

#[test]
fn to_tag_marks_in_dialog_and_admits() {
    assert!(!is_sheddable_new_invite(&req("INVITE", "To: <sip:bob@example.com>;tag=b2\r\n")));
}

#[test]
fn to_header_name_casing_and_compact_form_are_recognised() {
    for tagless in ["TO: <sip:bob@h>\r\n", "to: <sip:bob@h>\r\n", "t: <sip:bob@h>\r\n"] {
        assert!(is_sheddable_new_invite(&req("INVITE", tagless)), "{tagless:?}");
    }
    for tagged in ["TO: <sip:bob@h>;TAG=B2\r\n", "t: <sip:bob@h>;tag=b2\r\n"] {
        assert!(!is_sheddable_new_invite(&req("INVITE", tagged)), "{tagged:?}");
    }
}

#[test]
fn non_invite_methods_are_admitted() {
    for m in ["ACK", "BYE", "CANCEL", "OPTIONS", "REGISTER"] {
        assert!(
            !is_sheddable_new_invite(&req(m, "To: <sip:bob@example.com>\r\n")),
            "{m} must be admitted"
        );
    }
}

#[test]
fn responses_are_admitted() {
    assert!(!is_sheddable_new_invite(
        b"SIP/2.0 200 OK\r\nTo: <sip:bob@h>\r\nCSeq: 1 INVITE\r\n\r\n"
    ));
    assert!(!is_sheddable_new_invite(b"SIP/2.0 180 Ringing\r\nTo: <sip:bob@h>\r\n\r\n"));
}

#[test]
fn emergency_invites_are_admitted() {
    // Each canonical r-value, mixed case, and a comma-separated list.
    for rph in ["esnet.0", "wps.0", "q735.0", "ESNET.0", "Wps.0", "dsn.flash, q735.0"] {
        let raw = req("INVITE", &format!("To: <sip:bob@h>\r\nResource-Priority: {rph}\r\n"));
        assert!(!is_sheddable_new_invite(&raw), "{rph:?} must be admitted");
    }
    // Case-insensitive header name.
    assert!(!is_sheddable_new_invite(&req(
        "INVITE",
        "To: <sip:bob@h>\r\nRESOURCE-PRIORITY: esnet.0\r\n"
    )));
    // Any of multiple Resource-Priority headers flags.
    assert!(!is_sheddable_new_invite(&req(
        "INVITE",
        "To: <sip:bob@h>\r\nResource-Priority: dsn.flash\r\nResource-Priority: wps.0\r\n"
    )));
}

#[test]
fn non_emergency_resource_priority_stays_sheddable() {
    // r-values compare whole (comma-split, trimmed) — mirrors
    // `crate::emergency`: `dsn.flash` and the embedded `esnet.01` are not
    // emergency, so the new INVITE remains sheddable.
    for rph in ["dsn.flash", "esnet.01"] {
        let raw = req("INVITE", &format!("To: <sip:bob@h>\r\nResource-Priority: {rph}\r\n"));
        assert!(is_sheddable_new_invite(&raw), "{rph:?} is not emergency");
    }
}

#[test]
fn truncated_and_garbage_datagrams_are_admitted() {
    for raw in [
        &b""[..],
        &b"INVITE"[..],
        &b"INVITE sip:bob@h SIP/2.0\r\nVia: SIP/2.0/UDP x\r\n\r\n"[..], // no To at all
        &b"invite sip:bob@h SIP/2.0\r\nTo: <sip:bob@h>\r\n\r\n"[..],    // method case-sensitive
        &b"\x00\x01\x02 garbage \xff\xfe"[..],
    ] {
        assert!(!is_sheddable_new_invite(raw), "{raw:?} must be admitted");
    }
}

#[test]
fn trace_sample_rate_reads_a_float_in_range_off_the_raw_invite() {
    for (wire, expected) in [("1", 1.0), ("0", 0.0), ("0.25", 0.25), ("  0.5 ", 0.5)] {
        let raw = req("INVITE", &format!("X-Trace-Sample: {wire}\r\n"));
        assert_eq!(trace_sample_rate(&raw), TraceSample::Rate(expected), "{wire}");
    }
}

#[test]
fn trace_sample_rate_matches_the_full_parse_readers_grammar() {
    // Absent and malformed stay apart, out-of-range is refused, the first
    // instance wins, and the header name is case-insensitive — the same
    // contract `trace_sample::trace_sample` states over a parsed request.
    assert_eq!(trace_sample_rate(&req("INVITE", "")), TraceSample::Absent);
    for bad in ["yes", "1.5", "-0.1", "NaN", "inf", "0.5,0.9", ""] {
        let raw = req("INVITE", &format!("X-Trace-Sample: {bad}\r\n"));
        assert_eq!(trace_sample_rate(&raw), TraceSample::Malformed, "{bad:?}");
    }
    let repeated = req("INVITE", "X-Trace-Sample: 0.1\r\nX-Trace-Sample: 1\r\n");
    assert_eq!(trace_sample_rate(&repeated), TraceSample::Rate(0.1));
    let lower = req("INVITE", "x-trace-sample: 0.75\r\n");
    assert_eq!(trace_sample_rate(&lower), TraceSample::Rate(0.75));
}

#[test]
fn an_over_long_value_is_malformed_by_length_alone() {
    let raw = req("INVITE", &format!("X-Trace-Sample: 0.{}\r\n", "1".repeat(64)));
    assert_eq!(trace_sample_rate(&raw), TraceSample::Malformed);
}

#[test]
fn only_an_invite_datagram_is_scanned() {
    for m in ["ACK", "BYE", "CANCEL", "OPTIONS", "REGISTER"] {
        let raw = req(m, "X-Trace-Sample: 1\r\n");
        assert_eq!(trace_sample_rate(&raw), TraceSample::Absent, "{m} opens no trace");
    }
    assert_eq!(
        trace_sample_rate(b"SIP/2.0 200 OK\r\nX-Trace-Sample: 1\r\n\r\n"),
        TraceSample::Absent,
    );
}

#[test]
fn a_body_carrying_the_header_name_is_not_scanned() {
    let mut raw = req("INVITE", "");
    raw.extend_from_slice(b"X-Trace-Sample: 1\r\n");
    assert_eq!(trace_sample_rate(&raw), TraceSample::Absent, "the scan stops at the body");
}

#[test]
fn header_values_reads_every_row_in_wire_order() {
    let raw = b"CANCEL sip:x SIP/2.0\r\n\
                    Route: <sip:p1@h;lr>\r\n\
                    Route: <sip:p2@h;lr>, <sip:p3@h;lr>\r\n\
                    Call-ID: c1\r\n\r\nRoute: <sip:body@h>\r\n";
    assert_eq!(
        header_values(raw, "route"),
        vec!["<sip:p1@h;lr>".to_string(), "<sip:p2@h;lr>, <sip:p3@h;lr>".to_string()],
        "one entry per row, comma folds kept whole, the scan stops at the body",
    );
    assert!(header_values(raw, "record-route").is_empty(), "an absent header reads empty");
}

#[test]
fn header_values_reads_the_compact_spelling_too() {
    // `m` is Contact (RFC 3261 §20): a REGISTER's binding is the sender's
    // whichever name it wrote, so both rows are read, in wire order.
    let raw = b"REGISTER sip:h SIP/2.0\r\n\
                    Contact: <sip:a@1>\r\n\
                    m: <sip:a@2>\r\n\
                    Call-ID: c1\r\n\r\n";
    assert_eq!(
        header_values(raw, "contact"),
        vec!["<sip:a@1>".to_string(), "<sip:a@2>".to_string()],
    );
}

#[test]
fn bottom_via_reads_the_last_value_of_the_last_row() {
    let raw = b"INVITE sip:x SIP/2.0\r\n\
                    Via: SIP/2.0/UDP p.example;branch=z9-p\r\n\
                    v: SIP/2.0/TCP q.example:5070;branch=z9-q, SIP/2.0/UDP UA.Example:5062;branch=z9-ua\r\n\r\n";
    let origin = bottom_via(raw).expect("a bottom Via");
    let sent_by = origin.sent_by.as_borrowed();
    assert_eq!((sent_by.host(), sent_by.port()), ("UA.Example", Some(5062)));
    assert_eq!((origin.branch.as_deref(), origin.depth), (Some("z9-ua"), 3));
    let one = b"INVITE sip:x SIP/2.0\r\nVia: SIP/2.0/UDP ua.example\r\n\r\n";
    let origin = bottom_via(one).expect("one Via is its own bottom");
    assert_eq!(origin.branch, None);
    assert_eq!((origin.sent_by.as_borrowed().host(), origin.depth), ("ua.example", 1));
    assert_eq!(bottom_via(b"ACK sip:x SIP/2.0\r\n\r\n"), None);
}

#[test]
fn via_branch_takes_topmost_via_only() {
    let raw = b"INVITE sip:x SIP/2.0\r\nVia: SIP/2.0/UDP a;branch=z9-top\r\nVia: SIP/2.0/UDP b;branch=z9-bot\r\n\r\n";
    assert_eq!(via_branch(raw).as_deref(), Some("z9-top"));
    assert_eq!(via_branch(b"ACK sip:x SIP/2.0\r\n\r\n"), None);
}

#[test]
fn has_header_reads_the_compact_spelling_and_an_empty_value() {
    let raw = b"INVITE sip:x SIP/2.0\r\nAllow: INVITE, ACK\r\nk: 100rel\r\n\r\n";
    assert!(has_header(raw, "Allow"));
    assert!(has_header(raw, "Supported"), "the `k` compact form is the Supported header");
    assert!(!has_header(raw, "Require"));
    assert!(
        has_header(b"INVITE sip:x SIP/2.0\r\nSupported:\r\n\r\n", "Supported"),
        "an empty value is still an advertised header",
    );
}

#[test]
fn name_addr_uri_drops_the_headers_own_parameters() {
    let raw = b"BYE sip:x SIP/2.0\r\n\
                    From: \"A\" <sip:alice@atlanta.com>;tag=at\r\n\
                    t: sip:bob@biloxi.com;tag=bt\r\n\r\n";
    assert_eq!(name_addr_uri(raw, "From").as_deref(), Some("sip:alice@atlanta.com"));
    assert_eq!(
        name_addr_uri(raw, "To").as_deref(),
        Some("sip:bob@biloxi.com"),
        "a bare addr-spec's `;tag=` is the header's parameter, not the URI's",
    );
    assert_eq!(name_addr_uri(raw, "Contact"), None, "an absent header reads None");
}

#[test]
fn address_user_reads_an_address_value() {
    assert_eq!(address_user("sip:+15550100@pool.example").as_deref(), Some("+15550100"));
    assert_eq!(
        address_user("\"Pool\" <sip:+15550100@pool.example>;tag=1").as_deref(),
        Some("+15550100")
    );
    assert_eq!(address_user("sip:pool.example"), None, "a userless URI");
    assert_eq!(address_user("not an address"), None);
}

/// The user compares unescaped (RFC 3261 §19.1.4), and a tel URI's user is
/// its subscriber number before any `;` parameter.
#[test]
fn address_user_unescapes_and_reads_tel_uris() {
    assert_eq!(address_user("sip:%2B1555010@h").as_deref(), Some("+1555010"));
    assert_eq!(address_user("<sip:%2b1555%30%31@h>;tag=1").as_deref(), Some("+155501"));
    assert_eq!(address_user("tel:+1555010").as_deref(), Some("+1555010"));
    // Visual separators stay: the user is compared as written.
    assert_eq!(address_user("tel:+1-555-010").as_deref(), Some("+1-555-010"));
    assert_eq!(address_user("sip:+1-555-010@h").as_deref(), Some("+1-555-010"));
    assert_eq!(
        address_user("\"T\" <tel:+1555010;phone-context=example.com>;tag=2").as_deref(),
        Some("+1555010")
    );
    assert_eq!(
        from_user(b"INVITE sip:x@h SIP/2.0\r\nf: <tel:+1555010>;tag=3\r\n\r\n").as_deref(),
        Some("+1555010")
    );
}

#[test]
fn from_user_reads_the_from_uri_user_in_every_spelling() {
    let with = |from: &str| {
        format!("INVITE sip:x@h SIP/2.0\r\n{from}\r\nTo: <sip:other@h>\r\n\r\n").into_bytes()
    };
    let cases = [
        ("From: <sip:+1555010@a.example>;tag=1", "name-addr"),
        ("From: sip:+1555010@a.example;tag=1", "bare addr-spec"),
        ("f: <sip:+1555010@a.example>;tag=1", "compact form"),
        ("From: \"Caller Name\" <sip:+1555010@b.example:5070>;tag=9", "quoted display name"),
        ("From: Caller <sip:+1555010@c.example;user=phone>;tag=x", "token display name"),
    ];
    for (row, shape) in cases {
        assert_eq!(from_user(&with(row)).as_deref(), Some("+1555010"), "{shape}: {row}");
    }
    assert_eq!(from_user(&with("From: <sip:a.example>;tag=1")), None, "a userless URI");
    assert_eq!(
        from_user(b"INVITE sip:x@h SIP/2.0\r\nTo: <sip:+1555010@h>\r\n\r\n"),
        None,
        "an absent From reads None, whatever the To carries",
    );
}

#[test]
fn route_uris_splits_a_comma_fold_and_reads_loose_routing() {
    // One row, two hops; only the first advertises `;lr`, and `;lrx` is a
    // different parameter — a substring scan would call it loose.
    let raw = b"BYE sip:x SIP/2.0\r\n\
                    Route: <sip:p@host;lr>, <sip:q@h2>\r\n\
                    Route: <sip:r@h3:5080;lrx>\r\n\r\n";
    let hops = route_uris(raw, "Route").expect("every row reads");
    assert_eq!(hops.len(), 3, "{hops:?}");
    assert_eq!((hops[0].host.as_str(), hops[0].port), ("host", 5060));
    assert!(hops[0].loose);
    assert!(!hops[1].loose);
    assert_eq!((hops[2].host.as_str(), hops[2].port), ("h3", 5080));
    assert!(!hops[2].loose, "`;lrx` is not `;lr`");
    assert_eq!(
        route_uris(raw, "Record-Route"),
        Some(Vec::new()),
        "an absent header reads as no hops",
    );
    assert_eq!(
        route_uris(b"BYE sip:x SIP/2.0\r\nRoute: <sip:p@h;lr\r\n\r\n", "Route"),
        None,
        "a row no reader accepts names no hop",
    );
}

#[test]
fn route_rows_locate_each_row_in_the_raw_bytes() {
    let raw = b"INVITE sip:x SIP/2.0\r\n\
                    Via: SIP/2.0/UDP 10.0.0.9:5080;branch=z9hG4bK-p\r\n\
                    Record-Route: <sip:10.0.0.9:5080;w_bak=b2;lr>, <sip:10.0.0.7;lr>\r\n\
                    CSeq: 1 INVITE\r\n\
                    Record-Route: <sip:10.0.0.9:5080;lr\r\n\
                    Content-Length: 0\r\n\r\n\
                    Record-Route: <sip:body;lr>\r\n";
    let rows = route_rows(raw, "Record-Route");
    assert_eq!(rows.len(), 2, "the body's look-alike is past the blank line: {rows:?}");
    let text = |r: &RouteRow| String::from_utf8_lossy(&raw[r.span.clone()]).into_owned();
    assert_eq!(
        text(&rows[0]),
        "Record-Route: <sip:10.0.0.9:5080;w_bak=b2;lr>, <sip:10.0.0.7;lr>\r\n"
    );
    let hops = rows[0].hops.as_ref().expect("a readable row names its hops");
    assert_eq!(hops.len(), 2, "a comma fold is split: {hops:?}");
    assert_eq!((hops[0].host.as_str(), hops[0].port), ("10.0.0.9", 5080));
    assert_eq!((hops[1].host.as_str(), hops[1].port), ("10.0.0.7", 5060));
    assert_eq!(text(&rows[1]), "Record-Route: <sip:10.0.0.9:5080;lr\r\n");
    assert_eq!(rows[1].hops, None, "a row no reader accepts still has its span");
    assert!(route_rows(raw, "Route").is_empty(), "an absent header has no rows");
}

#[test]
fn request_uri_facts_resolve_the_default_port() {
    let facts = request_uri_facts(b"BYE sip:bob@10.0.0.2 SIP/2.0\r\n\r\n").expect("reads");
    assert_eq!((facts.host.as_str(), facts.port), ("10.0.0.2", 5060));
    assert_eq!(facts.uri, "sip:bob@10.0.0.2");
    assert_eq!(request_uri_facts(b"SIP/2.0 200 OK\r\n\r\n"), None, "a response has no Request-URI",);
}

#[test]
fn via_rport_keeps_the_four_states_apart() {
    let via = |v: &str| format!("OPTIONS sip:x SIP/2.0\r\nVia: {v}\r\n\r\n").into_bytes();
    assert_eq!(via_rport(&via("SIP/2.0/UDP h;branch=z9")), ViaRport::Absent);
    assert_eq!(via_rport(&via("SIP/2.0/UDP h;branch=z9;rport")), ViaRport::Requested);
    assert_eq!(via_rport(&via("SIP/2.0/UDP h;branch=z9;rport=5060")), ViaRport::Observed(5060),);
    assert_eq!(via_rport(&via("SIP/2.0/UDP h;branch=z9;rport=x")), ViaRport::Unreadable);
    assert_eq!(
        via_rport(b"OPTIONS sip:x SIP/2.0\r\nCall-ID: c\r\n\r\n"),
        ViaRport::Absent,
        "no Via names no rport",
    );
    let two = b"OPTIONS sip:x SIP/2.0\r\n\
                    Via: SIP/2.0/UDP top;branch=z9-t\r\n\
                    Via: SIP/2.0/UDP bot;branch=z9-b;rport=9\r\n\r\n";
    assert_eq!(via_rport(two), ViaRport::Absent, "only the top Via is read");
}

#[test]
fn body_presence_is_read_off_the_header_block() {
    let head = |extra: &str| format!("SIP/2.0 183 X\r\nCSeq: 1 INVITE\r\n{extra}\r\n").into_bytes();
    assert_eq!(content_length(&head("Content-Length: 42\r\n")), Some(42));
    assert_eq!(content_length(&head("l: 7\r\n")), Some(7), "compact spelling");
    assert_eq!(content_length(&head("Content-Length: x\r\n")), None, "unreadable");
    assert_eq!(content_length(&head("")), None, "absent");

    assert!(has_body(&head("Content-Length: 42\r\n")));
    assert!(!has_body(&head("Content-Length: 0\r\n")), "§20.14: bodiless states zero");
    assert!(!has_body(&head("")), "neither field: no body declared");
    assert!(
        has_body(&head("Content-Type: application/sdp\r\n")),
        "no readable length, but the message names the body it carries",
    );
    assert!(
        !has_body(&head("Content-Length: 0\r\nContent-Type: application/sdp\r\n")),
        "a readable length settles it",
    );
}

#[test]
fn body_is_what_follows_the_blank_line() {
    assert_eq!(body(b"INVITE sip:b SIP/2.0\r\nContent-Length: 3\r\n\r\nv=0"), Some(&b"v=0"[..]));
    assert_eq!(
        body(b"INVITE sip:b SIP/2.0\r\nContent-Length: 0\r\n\r\n"),
        Some(&b""[..]),
        "a terminated head with nothing after it carries an EMPTY body, not an unknown one"
    );
    assert_eq!(body(b"INVITE sip:b SIP/2.0\nl: 3\n\nv=0"), Some(&b"v=0"[..]), "bare LF");
    assert_eq!(body(b"INVITE sip:b SIP/2.0\r\nCSeq: 1 INVITE\r\n"), None, "unterminated");
}

/// ONE head-end rule: an empty line terminated by CR, LF or CRLF ends the
/// head, exactly where the parser's scanner ends its header block, so the
/// arm choice, the layout and the decode check never disagree on where a
/// body starts.
#[test]
fn the_head_ends_where_the_parser_s_scanner_ends_it() {
    use crate::parser::custom::scanner::header_block;
    let head_end = |raw: &[u8]| raw.len() - body(raw).expect("terminated").len();
    for raw in [
        &b"INFO sip:b SIP/2.0\r\nContent-Length: 3\r\n\r\nv=0"[..],
        b"INFO sip:b SIP/2.0\nContent-Length: 3\n\nv=0",
        b"INFO sip:b SIP/2.0\nContent-Length: 3\n\r\nv=0",
        b"INFO sip:b SIP/2.0\rContent-Length: 3\r\rv=0",
        b"INFO sip:b SIP/2.0\r\nContent-Length: 3\r\n\rv=0",
        b"INFO sip:b SIP/2.0\r\nContent-Length: 6\r\n\r\n\x00\xff\r\n\r\n",
    ] {
        assert_eq!(head_end(raw), header_block(raw).end, "{raw:?}");
        assert_eq!(body(raw), Some(&raw[header_block(raw).end..]), "{raw:?}");
    }
    assert_eq!(body(b"INFO sip:b SIP/2.0\rContent-Length: 3\r"), None, "unterminated by CR");
}

#[test]
fn a_multibyte_character_before_the_cseq_line_is_read_past() {
    let raw = "INVITE sip:bob@192.0.2.20 SIP/2.0\r\nabcd\u{e9}\r\nCSeq: 7 INVITE\r\n\r\n";
    assert_eq!(cseq_number(raw.as_bytes()), Some(7));
}

use super::*;

#[test]
fn harvests_ruri_identity_headers_and_displays_in_wire_order() {
    let raw = b"INVITE sip:+8355595552345678@example.net;user=phone SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
            From: \"MeetMe 85245\" <sip:85245@10.0.0.1>;tag=t1\r\n\
            To: <tel:+15552234567>\r\n\
            P-Asserted-Identity: <tel:+15552234567>, <sip:+15552234567@10.0.0.1>\r\n\
            Diversion: <sip:+15552345678@example.net>;reason=unknown\r\n\
            Contact: <sip:+15552234567.iIiIiI.500cc7a7.@10.0.0.1:5060>\r\n\
            History-Info: <sip:+15553234567@example.net>;index=1\r\n\
            Refer-To: <sip:+15556345678@10.0.0.9:5060?User-to-User=00ab>\r\n\
            Referred-By: \"Desk 41001\" <sip:41001@10.0.0.1>\r\n\
            P-Example-Hist: <sip:+15552765432@10.0.0.7>;exh=1, <sip:+15552765433@10.0.0.7>\r\n\
            X-Callback: tel:+15551543210;phone-context=x\r\n\
            Route: <sip:10.0.0.5;lr>, <sip:+15551222333@10.0.0.6;lr>\r\n\
            User-to-User: 00ab12345678;encoding=hex\r\n\
            Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    assert_eq!(
        h.numbers,
        vec![
            "+8355595552345678",
            "85245",
            "+15552234567",
            "+15552234567",
            "+15552234567",
            "+15552345678",
            "+15552234567.iIiIiI.500cc7a7.",
            "+15553234567",
            "+15556345678",
            "41001",
            "+15552765432",
            "+15552765433",
            "+15551543210",
            "+15551222333",
        ]
    );
    assert_eq!(h.displays, vec![b"MeetMe 85245".to_vec(), b"Desk 41001".to_vec()]);
    assert!(h.passports.is_empty());
    assert_eq!(
        h.uuis,
        vec![
            Uui { payload: "00ab".into(), encoding: UuiEncoding::Other },
            Uui { payload: "00ab12345678".into(), encoding: UuiEncoding::Hex }
        ],
        "the Refer-To's escaped header carries no encoding; the header form says hex"
    );
}

/// The User-to-User payload is harvested from the header form and from
/// the escaped URI header of any URI (Contact, Refer-To, a vendor header;
/// a Request-URI carries none, RFC 3261 §19.1.1), with its encoding read
/// case-insensitively: `hex`, `ascii`/`text`, `base64`, or other (absent,
/// unknown).
#[test]
fn user_to_user_payloads_are_harvested_from_headers_and_uri_headers() {
    let raw = b"INVITE sip:+15552345678@example.net SIP/2.0\r\n\
            Contact: <sip:10.0.0.1:5060?User-to-User=0430%3BEncoding%3DHex>\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
            From: <sip:+15552234567@10.0.0.1>;tag=t1\r\n\
            To: <sip:+15552345678@example.net>\r\n\
            Refer-To: <sip:+15556345678@10.0.0.9?User-to-User=0431%3Bencoding%3Dhex%3Bpurpose%3Disdn-uui>\r\n\
            P-Vendor: <sip:+15556345679@10.0.0.9?User-to-User=0432%3Bencoding%3Dhex>\r\n\
            User-to-User: 0433;encoding=hex;purpose=isdn-uui\r\n\
            User-to-User: abc;encoding=text\r\n\
            User-to-User: a1;encoding=ASCII\r\n\
            User-to-User: 0434\r\n\
            User-to-User: MDQ=;encoding=base64\r\n\
            Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    let uui = |payload: &str, encoding: UuiEncoding| Uui { payload: payload.into(), encoding };
    use UuiEncoding::{Base64, Hex, Other, Text};
    assert_eq!(
        h.uuis,
        vec![
            uui("0430", Hex),
            uui("0431", Hex),
            uui("0432", Hex),
            uui("0433", Hex),
            uui("abc", Text),
            uui("a1", Text),
            uui("0434", Other),
            uui("MDQ=", Base64)
        ]
    );
    assert_eq!(
        h.numbers,
        vec!["+15552345678", "+15552234567", "+15552345678", "+15556345678", "+15556345679"]
    );
}

/// An unbracketed addr-spec carrying escaped headers (a `?` RFC 3261 §20
/// puts in brackets, so the name-addr grammar refuses the line) is still read
/// as a whole URI: its number, its host and its User-to-User payload.
#[test]
fn a_bare_addr_spec_with_escaped_headers_yields_its_uui_and_host() {
    let raw =
        b"REFER sip:+15552345678@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
            Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK1\r\n\
            From: <sip:+15552234567@192.0.2.1>;tag=t1\r\n\
            To: <sip:+15552345678@example.net>;tag=t2\r\n\
            Refer-To: sip:+15556345678@refer.example.net?User-to-User=0431%3Bencoding%3Dhex\r\n\
            Call-ID: c1\r\nCSeq: 2 REFER\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    assert_eq!(h.uuis, vec![Uui { payload: "0431".into(), encoding: UuiEncoding::Hex }]);
    assert!(h.numbers.iter().any(|n| n == "+15556345678"), "the number: {:?}", h.numbers);
    assert!(
        h.names.hosts.iter().any(|n| n == "refer.example.net"),
        "the host: {:?}",
        h.names.hosts
    );
}

/// An `Identity` header carrying a compact PASSporT yields its payload
/// segment, verbatim; the RFC 4474 quoted form and a malformed token
/// yield nothing, and the header's own URIs (`info=`) are not identities.
#[test]
fn identity_headers_yield_their_passport_payload() {
    let raw = b"INVITE sip:+15552345678@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
            From: <sip:+15552234567@10.0.0.1>;tag=t1\r\n\
            To: <sip:+15552345678@example.net>\r\n\
            Identity: eyJhbGciOiJFUzI1NiIsInBwdCI6InNoYWtlbiIsInR5cCI6InBhc3Nwb3J0IiwieDV1IjoiaHR0cHM6Ly9jZXJ0LmV4YW1wbGUvYS5wZW0ifQ.\
eyJhdHRlc3QiOiJBIiwiZGVzdCI6eyJ0biI6WyIrMTU1NTIzNDU2NzgiXX0sImlhdCI6MTQ0MzIwODM0NSwib3JpZyI6eyJ0biI6IisxNTU1MjIzNDU2NyJ9LCJvcmlnaWQiOiJkZTMwNWQ1NC03NWI0LTQzMWItYWRiMi1lYjZiOWU1NDYwMTQifQ.\
rq3pjT1hoRwakEGjHCnWSwUnshd0-zJ6F1VOgFWSjHBr8Qjpjlk-cpFYpFYsojNCpTzO3QfPOlckGaS6hEck7w;\
info=<https://cert.example/a.pem>;alg=ES256;ppt=shaken\r\n\
            Identity: \"ZmFrZQ==\"\r\n\
            Identity: abc.def;info=<https://cert.example/b.pem>\r\n\
            Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    assert_eq!(h.numbers, vec!["+15552345678", "+15552234567", "+15552345678"]);
    assert_eq!(
        h.passports,
        vec!["eyJhdHRlc3QiOiJBIiwiZGVzdCI6eyJ0biI6WyIrMTU1NTIzNDU2NzgiXX0sImlhdCI6MTQ0MzIwODM0NSwib3JpZyI6eyJ0biI6IisxNTU1MjIzNDU2NyJ9LCJvcmlnaWQiOiJkZTMwNWQ1NC03NWI0LTQzMWItYWRiMi1lYjZiOWU1NDYwMTQifQ"]
    );
}

/// A display name is harvested as the datagram writes it: a quoted one
/// between its quotes with its escapes, so the rewrite finds it on the
/// wire; a bare one as its own tokens.
#[test]
fn a_display_name_is_harvested_as_written() {
    let raw =
        b"INVITE sip:+15552345678@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
            From: \"Fakey \\\"FF\\\" McFakename\" <sip:85245@10.0.0.1>;tag=t1\r\n\
            To: Desk  Two <sip:+15552234567@10.0.0.1>\r\n\
            Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    assert_eq!(h.displays, vec![b"Fakey \\\"FF\\\" McFakename".to_vec(), b"Desk  Two".to_vec()]);
}

#[test]
fn urn_identities_yield_no_number() {
    let raw = b"INVITE urn:service:sos.ecall.manual SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
            From: <sip:anonymous@anonymous.invalid>;tag=t1\r\n\
            To: \"urn:service:sos.ecall.manual\" <urn:service:sos.ecall.manual>\r\n\
            Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    assert!(h.numbers.is_empty(), "{:?}", h.numbers);
    assert_eq!(h.names.texts, vec!["anonymous"], "a letter-led user is a name");
    assert_eq!(h.displays, vec![b"urn:service:sos.ecall.manual".to_vec()]);
}

/// A compact-form identity header is the header it names, so its addresses
/// are harvested like any other.
#[test]
fn compact_identity_headers_are_harvested() {
    let raw = b"INVITE sip:+15552345678@example.net SIP/2.0\r\n\
            v: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
            f: \"Fakey\" <sip:85245@10.0.0.1>;tag=t1\r\n\
            t: <sip:+15552234567@10.0.0.1>\r\n\
            m: <sip:+15552234567@10.0.0.1:5060>\r\n\
            i: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nl: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    assert_eq!(h.numbers, vec!["+15552345678", "85245", "+15552234567", "+15552234567"]);
    assert_eq!(h.displays, vec![b"Fakey".to_vec()]);
}

/// An unbracketed addr-spec whose userinfo carries a `;` (legal per
/// RFC 3261 §19.1.1 `user-unreserved`) still yields its number: the
/// name-addr grammar reads that tail as header parameters (§20.10), so the
/// bare-addr-spec re-read is what keeps the identity discoverable — and
/// anonymizable.
#[test]
fn a_bare_escaped_userinfo_address_still_yields_its_number() {
    let raw = b"INVITE sip:+15553234567;exlevel=x@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
            From: <sip:+15553234567;exlevel=x@10.0.0.1>;tag=t1\r\n\
            To: <sip:+15553234567@example.net>\r\n\
            P-Asserted-Identity: sip:+15553234567;exlevel=x@10.0.0.1\r\n\
            Diversion: sip:+15553234567;exlevel=x@10.0.0.1;reason=unknown\r\n\
            Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    assert_eq!(
        h.numbers,
        vec![
            "+15553234567;exlevel=x",
            "+15553234567;exlevel=x",
            "+15553234567",
            "+15553234567;exlevel=x",
            "+15553234567;exlevel=x"
        ]
    );
    assert!(h.displays.is_empty());
}

/// The bare-addr-spec re-read never double-counts an address the name-addr
/// grammar already read: an unbracketed addr-spec with an ordinary tag
/// contributes its user exactly once.
#[test]
fn a_bare_addr_spec_with_a_tag_is_harvested_once() {
    let raw = b"INVITE sip:+15552345678@example.net SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
            From: sip:85245@10.0.0.1;tag=t1\r\n\
            To: tel:+15552234567;tag=t2\r\n\
            Contact: <sip:10.0.0.9:5060>;expires=300\r\n\
            Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    assert_eq!(h.numbers, vec!["+15552345678", "85245", "+15552234567"]);
}

/// The re-read runs on the offending address's own comma entry, so a
/// multi-address line harvests an escaped userinfo wherever it sits and
/// harvests its siblings exactly once.
#[test]
fn a_multi_value_line_harvests_each_address_on_its_own() {
    let raw =
        b"INVITE sip:+15552345678@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
            From: <sip:85245@10.0.0.1>;tag=t1\r\n\
            To: <sip:+15552345678@example.net>\r\n\
            Diversion: sip:85245@10.0.0.1;reason=unknown, \
            sip:+15553234567;exlevel=x@10.0.0.2;reason=unknown\r\n\
            History-Info: <sip:+15553234567;exlevel=x@10.0.0.2>;index=1, \
            sip:+15556345678;exlevel=y@10.0.0.3;index=1.1\r\n\
            Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let h = harvest(raw, &BodyTypes::default());
    assert_eq!(
        h.numbers,
        vec![
            "+15552345678",
            "85245",
            "+15552345678",
            "85245",
            "+15553234567;exlevel=x",
            "+15553234567;exlevel=x",
            "+15556345678;exlevel=y"
        ]
    );
}

/// A BYE to `sip:{user}@fake-sbc.notreal-carrier.de`.
fn bye_to(user: &str) -> String {
    format!(
        "BYE sip:{user}@fake-sbc.notreal-carrier.de SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK1\r\n\
         From: <sip:+15556345678@fake-sbc.notreal-carrier.de>;tag=t1\r\n\
         To: <sip:{user}@fake-sbc.notreal-carrier.de>;tag=t2\r\n\
         Call-ID: c1\r\nCSeq: 2 BYE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    )
}

/// A user part that is a dotted quad is an address, the address pass's: it
/// yields no number and no separated spelling.
#[test]
fn a_dotted_quad_user_part_is_no_number() {
    for user in ["10.20.30.40", "192.0.2.10", "0.31.5.180", "247.31.5.180"] {
        let h = harvest(bye_to(user).as_bytes(), &BodyTypes::default());
        assert_eq!(h.numbers, vec!["+15556345678"], "{user}");
        assert!(h.separated.is_empty(), "{user}: {:?}", h.separated);
    }
}

/// A dotted user part that is no quad as written — three or five groups, a
/// group over 255, an octet with a leading zero (a trunk-prefixed national
/// number), an escaped dot — is a number written with separators (RFC 3966
/// §5.1.1) or escapes.
#[test]
fn a_dotted_user_part_that_is_no_quad_stays_a_number() {
    for (user, digits) in [
        ("06.23.45.67.8", "062345678"),
        ("062.345.678", "062345678"),
        ("155.563.456.78", "15556345678"),
        ("010.123.45.6", "010123456"),
        ("02.123.45.67", "021234567"),
        ("10%2E20.30.40", "10203040"),
    ] {
        let h = harvest(bye_to(user).as_bytes(), &BodyTypes::default());
        assert_eq!(h.numbers, vec![digits, "+15556345678", digits], "{user}");
        assert_eq!(h.separated, vec![user, user], "{user}");
    }
}

/// A separated number carrying user parameters (RFC 4694 `;npdi;rn=`) is read
/// plain with its parameters behind it; its spelling is the number's alone.
#[test]
fn a_separated_number_carrying_user_parameters_is_read_plain() {
    for (user, number, spelling) in [
        ("+1-555-676-5432;npdi", "+15556765432;npdi", "+1-555-676-5432"),
        ("06.87.65.43.2;npdi", "068765432;npdi", "06.87.65.43.2"),
        (
            "+1-555-676-5432;rn=+15556223344;npdi",
            "+15556765432;rn=+15556223344;npdi",
            "+1-555-676-5432",
        ),
    ] {
        let h = harvest(bye_to(user).as_bytes(), &BodyTypes::default());
        assert_eq!(h.numbers, vec![number, "+15556345678", number], "{user}");
        assert_eq!(h.separated, vec![spelling, spelling], "{user}");
    }
}

/// A sipfrag and a control document are read for the URIs, network hosts and
/// message ids their text holds.
#[test]
fn an_understood_body_part_yields_its_uris_and_hosts() {
    let frag = "SIP/2.0 200 OK\r\nFrom: <sip:+15556345678@frag-host.zz>;tag=1\r\n";
    let control = "<msml><play><audio uri=\"https://media.ctl-host.zz/a.wav\"/></play></msml>";
    for (media, body) in
        [("message/sipfrag", frag), ("application/mediaservercontrol+xml", control)]
    {
        let raw = format!(
            "NOTIFY sip:x@h SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\nVia: SIP/2.0/UDP 10.0.0.1;branch=z9hG4bK1\r\n\
             From: <sip:a@h>;tag=t1\r\nTo: <sip:x@h>;tag=t2\r\nCall-ID: c1\r\nCSeq: 2 NOTIFY\r\n\
             Max-Forwards: 70\r\nContent-Type: {media}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let h = harvest(raw.as_bytes(), &BodyTypes::default());
        if media == "message/sipfrag" {
            assert!(h.numbers.contains(&"+15556345678".to_string()), "{:?}", h.numbers);
            assert!(h.names.hosts.contains(&"frag-host.zz".to_string()), "{:?}", h.names.hosts);
        } else {
            assert!(
                h.names.hosts.contains(&"media.ctl-host.zz".to_string()),
                "{:?}",
                h.names.hosts
            );
        }
    }
}

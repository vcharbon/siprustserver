//! The anonymiser through the library path the binary runs (ingest →
//! anonymise → emit → ingest) on crafted INVITEs under the invented plan:
//! every number of the raw document is gone under every composed form, the
//! output reproduces byte for byte under the kept seed, is refused another
//! pass, and the leak checks count nothing.

use pcap_anon::fake::address::Scope;
use pcap_anon::fake::{fake_for, Seed, GOLDEN_SEED_HEX};
use pcap_anon::{anonymize, model, plan, uui, verify};

use crate::support::{plan as test_plan, reading};

/// The seed every expectation on concrete fake digits derives them from,
/// through `fake_for`.
fn seed() -> Seed {
    Seed::from_hex(GOLDEN_SEED_HEX).unwrap()
}

/// Every datagram's raw bytes concatenated.
fn corpus_bytes(flows: &model::Flows) -> Vec<u8> {
    let mut blob = Vec::new();
    for leg in &flows.legs {
        for msg in &leg.msgs {
            blob.extend_from_slice(&model::wire(msg));
        }
    }
    blob
}

/// Whether `needle` occurs anywhere in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// A flows document with one INVITE (`raw` bytes + a matching summary),
/// serialised exactly as the binary emits it.
fn single_request_flows_json(raw: &[u8], uri: &str) -> String {
    let skeleton = serde_json::json!({
        "schema": model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 1, "non_ip": 0, "non_udp": 0, "snap_truncated": 0,
                          "datagrams": 1, "fragments": 0, "reassembled": 0, "frag_dropped": 0,
                          "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": "c1", "hops": [{ "a": "10.0.0.1:5060", "b": "10.0.0.2:5060" }],
                   "invite": null, "final_status": null, "saw_180": false,
                   "terminated_by": null, "tokens": [], "msgs": [] }],
        "groups": [{ "legs": [0], "evidence": [] }]
    });
    let mut flows = model::ingest_str(&skeleton.to_string()).expect("the skeleton ingests");
    let summary = model::Summary::Request {
        method: "INVITE".into(),
        uri: uri.into(),
        cseq: model::CSeq { seq: 1, method: "INVITE".into() },
        from: model::Party { uri: "sip:+2821234567@10.0.0.1".into(), tag: Some("t1".into()) },
        to: model::Party { uri: uri.into(), tag: None },
    };
    flows.legs[0].msgs.push(model::Msg::new(
        1,
        "10.0.0.1:5060".into(),
        "10.0.0.2:5060".into(),
        0,
        sip_pcap::doc::Payload::of_datagram(raw),
        summary,
    ));
    model::to_json_string(&flows, &reading()).expect("the crafted message re-enriches")
}

/// The binary's contract on one raw document: anonymised from its JSON twice
/// under the golden seed, the two emitted documents are byte-identical; the
/// output, read back, is refused another pass; and the leak checks count
/// nothing. Returns the emitted document.
fn assert_reproducible_and_refused(flows_json: &str, plan: &plan::Plan) -> String {
    let raw = model::ingest_str(flows_json).unwrap();
    let run = || {
        let mut flows = model::ingest_str(flows_json).unwrap();
        let report = anonymize::anonymize(&mut flows, plan, &seed()).unwrap();
        (model::to_json_string(&flows, &reading()).unwrap(), flows, report)
    };
    let ((once_json, once, report), (twice_json, _, _)) = (run(), run());
    assert_eq!(once_json, twice_json, "the kept seed reproduces the output");
    let scope = Scope::default();
    assert_eq!(verify::survivors(&raw, &once, plan, &scope), 0);
    assert_eq!(verify::unmasked_displays(&once, &scope), 0);
    assert_eq!(verify::names::name_survivors(&raw, &once, &scope), 0);
    let leaks = verify::leaks::leaks(&once, &scope, &report.fakes, &reading());
    assert_eq!(leaks.total(), 0, "{leaks:?}");
    let mut again = model::ingest_str(&once_json).unwrap();
    let refused = anonymize::anonymize(&mut again, plan, &seed());
    assert!(matches!(refused, Err(anonymize::AnonymizeError::AlreadyRewritten(_))));
    once_json
}

/// A display run of junk digits followed by a discovered NSN
/// (`cf 9921234567`) is masked and reproduces byte for byte.
#[test]
fn mixed_junk_plus_nsn_display_run_reproduces_through_the_binary_path() {
    let raw: &[u8] = b"INVITE sip:+2821234567@h SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
        From: \"cf 9921234567\" <sip:+2821234567@10.0.0.1>;tag=t1\r\n\
        To: <sip:+2821234567@h>\r\n\
        Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let flows_json = single_request_flows_json(raw, "sip:+2821234567@h");
    let plan = test_plan();

    let mut once = model::ingest_str(&flows_json).unwrap();
    let first = anonymize::anonymize(&mut once, &plan, &seed()).unwrap();
    assert_eq!(first.entries.len(), 1, "the NSN is discovered");
    let once_json = model::to_json_string(&once, &reading()).unwrap();
    let masked = format!("\"xx 00{}\"", first.entries[0].fake);
    assert!(find(&corpus_bytes(&once), masked.as_bytes()), "the junk zeroed, the NSN its fake");
    assert_eq!(assert_reproducible_and_refused(&flows_json, &plan), once_json);
}

/// A number composed into a longer form (`+CC <NSN> <junk>`), whatever the
/// junk width (1..=8: every tail window the hash digits can leave), leaks
/// none of its digits and reproduces byte for byte.
#[test]
fn a_composed_tail_leaks_no_digit_through_the_binary_path() {
    let plan = test_plan();
    for junk in 1..=8 {
        let junk = &"23456789"[..junk];
        let uri = format!("sip:+2821234567{junk}@example.net");
        let raw = format!(
            "INVITE {uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
             From: <sip:+2821234567@10.0.0.1>;tag=t1\r\n\
             To: <{uri}>\r\n\
             Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\n\
             Contact: <sip:+2821234567@10.0.0.1:5060>\r\nContent-Length: 0\r\n\r\n"
        );
        let flows_json = single_request_flows_json(raw.as_bytes(), &uri);

        let once_json = assert_reproducible_and_refused(&flows_json, &plan);
        assert!(!once_json.contains("21234567"), "junk {junk}: NSN leaked");
    }
}

/// Nine five-digit private numbers and one NSN: the fakes the five-digit
/// ones share are counted, never refused, and the capture reproduces.
#[test]
fn many_same_length_privates_accepted_and_reproduced_through_the_binary_path() {
    let raw: &[u8] =
        b"INVITE sip:+2821234567@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
        From: <sip:41001@10.0.0.1>;tag=t1\r\n\
        To: <sip:+2821234567@example.net>\r\n\
        P-Asserted-Identity: <sip:41001@10.0.0.1>, <sip:41002@10.0.0.1>, <sip:41003@10.0.0.1>, \
<sip:41004@10.0.0.1>, <sip:41005@10.0.0.1>, <sip:41006@10.0.0.1>, <sip:41007@10.0.0.1>, \
<sip:41008@10.0.0.1>, <sip:41009@10.0.0.1>\r\n\
        Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let flows_json = single_request_flows_json(raw, "sip:+2821234567@example.net");
    let plan = test_plan();

    let mut once = model::ingest_str(&flows_json).unwrap();
    let report = anonymize::anonymize(&mut once, &plan, &seed()).expect("not refused");
    assert_eq!(report.entries.len(), 10, "9 privates + 1 NSN discovered");
    assert_eq!(
        assert_reproducible_and_refused(&flows_json, &plan),
        model::to_json_string(&once, &reading()).unwrap()
    );
}

/// An identity written as an unbracketed addr-spec whose userinfo carries a
/// `;` (legal per RFC 3261 §19.1.1, ambiguous per §20.10: the name-addr
/// grammar reads that tail as header parameters) is still harvested, its
/// number mapped and its userinfo parameter kept.
#[test]
fn bare_escaped_userinfo_identity_is_anonymized_through_the_binary_path() {
    let raw: &[u8] =
        b"INVITE sip:+2821234567@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
        From: <sip:+2821234567@10.0.0.1>;tag=t1\r\n\
        To: <sip:+2821234567@example.net>\r\n\
        P-Asserted-Identity: sip:+2823123456;exlevel=x@10.0.0.1\r\n\
        Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let flows_json = single_request_flows_json(raw, "sip:+2821234567@example.net");
    let plan = test_plan();

    let mut once = model::ingest_str(&flows_json).unwrap();
    let report = anonymize::anonymize(&mut once, &plan, &seed()).unwrap();
    assert_eq!(report.entries.len(), 2, "the R-URI NSN and the bare addr-spec NSN");
    let once_bytes = corpus_bytes(&once);
    assert!(!find(&once_bytes, b"23123456"), "the bare addr-spec NSN leaked");
    let pai =
        format!("P-Asserted-Identity: sip:+28{};exlevel=x@", fake_for(&plan, &seed(), "23123456"));
    assert!(find(&once_bytes, pai.as_bytes()), "the fake, the userinfo parameter intact");

    assert_reproducible_and_refused(&flows_json, &plan);
}

/// The same digit string as a bare private number and behind a `+` no CC
/// explains, and an NSN that also occurs CC-less: one key each, mapped
/// wholly into the reserved space whichever occurrence comes first.
#[test]
fn a_key_seen_under_two_classes_maps_wholly_reserved_in_any_order() {
    let plan = test_plan();
    for (first, second) in [("+8361234", "8361234"), ("8361234", "+8361234")] {
        let raw = format!(
            "INVITE sip:+2883512345@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
             From: <sip:{first}@10.0.0.1>;tag=t1\r\n\
             To: <sip:+2883512345@example.net>\r\n\
             P-Asserted-Identity: <sip:{second}@10.0.0.1>, <sip:+83512345@10.0.0.1>\r\n\
             Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        );
        let flows_json = single_request_flows_json(raw.as_bytes(), "sip:+2883512345@example.net");
        let mut once = model::ingest_str(&flows_json).unwrap();
        let report = anonymize::anonymize(&mut once, &plan, &seed()).unwrap();
        assert_eq!(report.entries.len(), 2, "{first}/{second}: one key each");
        assert_eq!(report.unrecognised(), 0, "{first}/{second}: the recognised reading wins");
        for e in &report.entries {
            assert!(e.fake.starts_with("0009"), "{first}/{second}: {} -> {}", e.original, e.fake);
        }
        let once_bytes = corpus_bytes(&once);
        assert!(!find(&once_bytes, b"8361234") && !find(&once_bytes, b"83512345"));
        assert_reproducible_and_refused(&flows_json, &plan);
    }
}

/// Every digit-leak class in one INVITE: a transfer target in Refer-To and
/// Referred-By, numbers of the plan's other countries, a short code, an
/// unknown CC of 14 digits, a 21-digit oddity, a 4-digit `tel:`, a vendor
/// header, a route-set URI with a user. After one pass none of the raw digit
/// strings of at least 4 digits is in the bytes, `--verify`'s check agrees,
/// and the check against the raw document itself is loud.
#[test]
fn no_raw_digit_string_survives_whatever_the_header_or_the_shape() {
    let raw: &[u8] = b"INVITE sip:+15552123456@example.net SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
        From: \"Desk\" <sip:+2839121@10.0.0.1>;tag=t1\r\n\
        To: <sip:+15552123456@example.net>\r\n\
        P-Asserted-Identity: <sip:+83571234567890@10.0.0.1>\r\n\
        Diversion: <sip:+062237020000900123456@10.0.0.1>;reason=unknown\r\n\
        Refer-To: <sip:+2829876543@10.0.0.9:5060?User-to-User=00ab>\r\n\
        Referred-By: <sip:+899123456789012@10.0.0.1>\r\n\
        P-Example-Hist: <sip:+2817654321@10.0.0.7>;exh=1\r\n\
        X-Callback: <tel:2210>\r\n\
        Route: <sip:+15554123456@10.0.0.6;lr>\r\n\
        Contact: <sip:1234abc@10.0.0.1>\r\n\
        Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
    let flows_json = single_request_flows_json(raw, "sip:+15552123456@example.net");
    let plan = test_plan();
    let before = model::ingest_str(&flows_json).unwrap();

    let mut once = model::ingest_str(&flows_json).unwrap();
    let report = anonymize::anonymize(&mut once, &plan, &seed()).unwrap();
    let originals: Vec<&str> = report.entries.iter().map(|e| e.original.as_str()).collect();
    let digits = [
        "5552123456",
        "39121",
        "83571234567890",
        "062237020000900123456",
        "29876543",
        "123456789012",
        "17654321",
        "2210",
        "5554123456",
        "1234",
    ];
    assert_eq!(originals, digits);
    assert_eq!(report.unrecognised(), 3);
    let once_bytes = corpus_bytes(&once);
    for digits in digits.iter().map(|d| d.trim_start_matches('0')) {
        assert!(
            !find(&once_bytes, digits.as_bytes()),
            "{digits} survived: {}",
            String::from_utf8_lossy(&once_bytes)
        );
    }
    assert!(find(&once_bytes, b"<sip:+8350009"), "the unknown CC keeps three digits");
    let oddity = &report.entries[3].fake;
    assert!(oddity.starts_with("0620009"), "the oddity keeps three digits: {oddity}");
    assert!(find(&once_bytes, format!("<sip:+{oddity}@").as_bytes()), "{oddity}");
    assert!(find(&once_bytes, b"<sip:+280009"), "the short code is a 5-digit fake behind +28");
    assert!(find(&once_bytes, b"<tel:09"), "the 4-digit tel is a short fake");
    assert!(find(&once_bytes, b"<sip:09") && find(&once_bytes, b"abc@10.0.0.1>"), "junk kept");
    assert_eq!(verify::survivors(&before, &once, &plan, &Scope::default()), 0);
    assert_eq!(verify::survivors(&before, &before, &plan, &Scope::default()), 10);
    assert_reproducible_and_refused(&flows_json, &plan);
}

/// RFC 7433 UUI with `encoding=hex`, in the header form and in the escaped
/// URI header of a Refer-To: the decoded ASCII carries a national dial form
/// behind a one-byte discriminator. The header's number occurs nowhere else
/// and is discovered from the payload alone; the Refer-To's is the To's NSN,
/// so both hex payloads land on the same fake; upper case stays upper; an
/// `ascii`/`text` UUI is its own bytes, whose digit runs are keys rewritten
/// in place; a base64 one that decodes to no text is counted and not read;
/// the output reproduces byte for byte.
#[test]
fn a_hex_user_to_user_payload_is_rewritten_in_both_forms_and_reproduces() {
    const HDR: &str = "043036313233343536373B7075727068";
    const URI: &str = "04303231323334353637";
    let raw = format!(
        "INVITE sip:+2821234567@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
         From: <sip:+15552212345@10.0.0.1>;tag=t1\r\n\
         To: <sip:+2821234567@example.net>\r\n\
         Refer-To: <sip:+2821234567@10.0.0.9:5060?User-to-User={URI}%3Bencoding%3Dhex%3Bpurpose%3Disdn-uui>\r\n\
         User-to-User: {HDR};encoding=hex;purpose=isdn-uui\r\n\
         User-to-User: ab00577112233445cd|2210;encoding=ascii\r\n\
         User-to-User: 069876543 ref;encoding=text\r\n\
         User-to-User: BAMfAQ==;encoding=base64\r\n\
         Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    );
    let flows_json = single_request_flows_json(raw.as_bytes(), "sip:+2821234567@example.net");
    let plan = test_plan();
    let before = model::ingest_str(&flows_json).unwrap();

    let mut once = model::ingest_str(&flows_json).unwrap();
    let report = anonymize::anonymize(&mut once, &plan, &seed()).unwrap();
    let originals: Vec<&str> = report.entries.iter().map(|e| e.original.as_str()).collect();
    assert_eq!(
        originals,
        vec!["21234567", "5552212345", "61234567", "00577112233445", "2210", "69876543"]
    );
    assert_eq!(report.uui_skipped, 1, "the base64 payload is the one counted");
    let fake = |key: &str| fake_for(&plan, &seed(), key);
    let once_bytes = corpus_bytes(&once);
    for gone in ["21234567", "61234567", HDR, URI, "00577112233445", "2210", "69876543"] {
        assert!(!find(&once_bytes, gone.as_bytes()), "{gone} survived");
    }
    let short = fake("2210");
    let ascii = String::from_utf8_lossy(&once_bytes);
    let line = ascii.lines().find(|l| l.starts_with("User-to-User: ab")).unwrap_or("").to_string();
    assert!(line.starts_with("User-to-User: ab0050009"), "unrecognised head: {line}");
    assert!(find(&once_bytes, format!("cd|{short};encoding=ascii").as_bytes()));
    assert!(find(
        &once_bytes,
        format!("User-to-User: 0{} ref;encoding=text", fake("69876543")).as_bytes()
    ));
    assert!(find(&once_bytes, b"User-to-User: BAMfAQ==;encoding=base64"));
    let hdr = uui::rewrite(HDR, |b| {
        String::from_utf8_lossy(b).replace("61234567", &fake("61234567")).into_bytes()
    })
    .unwrap();
    let uri = uui::rewrite(URI, |b| {
        String::from_utf8_lossy(b).replace("21234567", &fake("21234567")).into_bytes()
    })
    .unwrap();
    assert_eq!(hdr.len(), HDR.len());
    assert!(hdr.contains("3B7075727068"), "the untouched tail keeps its upper case");
    assert!(find(&once_bytes, format!("User-to-User: {hdr};encoding=hex").as_bytes()), "{hdr}");
    assert!(find(&once_bytes, format!("?User-to-User={uri}%3Bencoding").as_bytes()), "{uri}");
    assert_eq!(uui::digit_runs(&uri), vec![format!("0{}", fake("21234567"))]);
    assert_eq!(verify::survivors(&before, &once, &plan, &Scope::default()), 0);
    assert_eq!(verify::survivors(&before, &before, &plan, &Scope::default()), 7);
    assert_reproducible_and_refused(&flows_json, &plan);
}

/// An `Identity` header carrying a compact PASSporT (RFC 8224): the
/// payload's `orig`/`dest` numbers ride the same mapping as the URIs — one of
/// them (`+2861234567`) occurs nowhere else and is discovered from the
/// payload alone — and the compact form is redacted whole, header, claims
/// and signature, same width: a kept signature would confirm a guessed
/// number. The parameters stay.
#[test]
fn a_passport_is_redacted_whole_and_its_numbers_map_through_the_same_mapping() {
    const HEAD: &str = "eyJhbGciOiJFUzI1NiIsInBwdCI6InNoYWtlbiIsInR5cCI6InBhc3Nwb3J0IiwieDV1IjoiaHR0cHM6Ly9jZXJ0LmV4YW1wbGUvYS5wZW0ifQ";
    const PAYLOAD: &str = "eyJhdHRlc3QiOiJBIiwiZGVzdCI6eyJ0biI6WyIrMjgyMTIzNDU2NyIsIisyODYxMjM0NTY3Il19LCJpYXQiOjE0NDMyMDgzNDUsIm9yaWciOnsidG4iOiIrMTU1NTIyMTIzNDUifSwib3JpZ2lkIjoiZGUzMDVkNTQtNzViNC00MzFiLWFkYjItZWI2YjllNTQ2MDE0In0";
    const SIG: &str =
        "rq3pjT1hoRwakEGjHCnWSwUnshd0-zJ6F1VOgFWSjHBr8Qjpjlk-cpFYpFYsojNCpTzO3QfPOlckGaS6hEck7w";
    let raw = format!(
        "INVITE sip:+2821234567@example.net SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
         From: <sip:+15552212345@10.0.0.1>;tag=t1\r\n\
         To: <sip:+2821234567@example.net>\r\n\
         Identity: {HEAD}.{PAYLOAD}.{SIG};info=<https://cert.example/a.pem>;alg=ES256;ppt=shaken\r\n\
         Call-ID: c1\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    );
    let flows_json = single_request_flows_json(raw.as_bytes(), "sip:+2821234567@example.net");
    let plan = test_plan();

    let mut once = model::ingest_str(&flows_json).unwrap();
    let report = anonymize::anonymize(&mut once, &plan, &seed()).unwrap();
    let originals: Vec<&str> = report.entries.iter().map(|e| e.original.as_str()).collect();
    assert_eq!(originals, vec!["21234567", "5552212345", "61234567"]);
    let once_bytes = corpus_bytes(&once);
    for gone in ["21234567", "5552212345", "61234567", HEAD, PAYLOAD, SIG] {
        assert!(!find(&once_bytes, gone.as_bytes()), "{gone} survived in the bytes");
    }
    let redacted = |part: &str| "_".repeat(part.len());
    let identity = format!(
        "Identity: {}.{}.{};info=<https://cert.example/a.pem>;alg=ES256;ppt=shaken",
        redacted(HEAD),
        redacted(PAYLOAD),
        redacted(SIG)
    );
    assert!(find(&once_bytes, identity.as_bytes()), "{}", String::from_utf8_lossy(&once_bytes));
    assert!(find(
        &once_bytes,
        format!("<sip:+1{}@", fake_for(&plan, &seed(), "5552212345")).as_bytes()
    ));
    assert_reproducible_and_refused(&flows_json, &plan);
}

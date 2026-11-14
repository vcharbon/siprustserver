//! A base64 run that decodes to text is that text standing in clear at the
//! same place: the rewrite writes into it what it writes into the clear
//! text there — numbers of any spelling, hosts, addresses, with the
//! exemptions of the place — the decoded text adds no key the clear text
//! would not add, and the leak check counts in it what it counts in the clear
//! text. Each probe anonymises a document twice, the text once in clear and
//! once encoded, and compares the two outputs. Every value is invented.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::engine::GeneralPurpose;
use base64::Engine as _;

use super::encoded_probe_tests::{anonymised, anonymised_with_report, between, doc, doc_of, wire};
use crate::fake::address::Scope;
use crate::model::Flows;

/// The header line of `wire` that opens like `place` does.
fn line_like(wire: &str, place: &str) -> String {
    let name = place.split(':').next().expect("a header name");
    let found = wire.lines().find(|line| line.split(':').next() == Some(name));
    found.expect("the probe's header line").to_string()
}

/// `text` written at `place` (a header line, `{}` where the text stands) in
/// a BYE from `from`, anonymised in clear and encoded by `engine`: what the
/// encoded run decodes to in its output, what the clear text became in its
/// own, and the two verdict totals.
fn in_clear_and_encoded(
    from: &str,
    place: &str,
    text: &str,
    engine: &GeneralPurpose,
) -> (String, String, usize, usize) {
    let at = place.find("{}").expect("a place for the text");
    let encoded = engine.encode(text);
    let (clear, clear_verdict) = anonymised(&doc(from, &[place.replace("{}", text)]));
    let (out, verdict) = anonymised(&doc(from, &[place.replace("{}", &encoded)]));
    let clear_line = line_like(&wire(&clear), place);
    let line = line_like(&wire(&out), place);
    let decoded = engine
        .decode(&line[at..at + encoded.len()])
        .unwrap_or_else(|e| panic!("{place} / {text}: the run is base64 of its alphabet: {e}"));
    (
        String::from_utf8(decoded).expect("the rewrite keeps the text a text"),
        clear_line[at..at + text.len()].to_string(),
        verdict,
        clear_verdict,
    )
}

/// Asserts `text` at `place` is rewritten encoded as it is in clear, mapped
/// or left as `maps` says, and judged alike.
fn assert_as_in_clear(from: &str, place: &str, text: &str, engine: &GeneralPurpose, maps: bool) {
    let (decoded, clear, verdict, clear_verdict) = in_clear_and_encoded(from, place, text, engine);
    assert_eq!(decoded, clear, "{place} / {text}: the decoded text is the clear text's rewrite");
    assert_eq!(decoded != text, maps, "{place} / {text}: {decoded}");
    assert_eq!(verdict, clear_verdict, "{place} / {text}: the leak check counts as in clear");
}

/// A tel URI's separators and a private number met as a user part map in
/// clear; a bare run narrower than a phone number, or a number spelt with
/// spaces or dots, in an unknown header stays in clear, and stays encoded.
#[test]
fn a_number_written_with_separators_or_a_short_one_is_handled_as_in_clear() {
    let param = "X-Ref: <sip:ref@ref-host.example.com;ident={}>";
    assert_as_in_clear("fakey", "X-Tok: {}", "tel:+1-555-634-5678", &STANDARD, true);
    assert_as_in_clear("54321", "X-Tok: {}", "ext=54321", &STANDARD, true);
    assert_as_in_clear("fakey", "X-Tok: {}", "ext=54321", &STANDARD, false);
    assert_as_in_clear("fakey", "X-Tok: {}", "+1 555 634 5678", &STANDARD, false);
    assert_as_in_clear("fakey", "X-Tok: {}", "06.87.65.43.2", &STANDARD, false);
    assert_as_in_clear("fakey", param, "+1 555 634 5678", &STANDARD, false);
}

#[test]
fn a_base64_run_of_utf8_text_is_read_as_text() {
    assert_as_in_clear("fakey", "X-Tok: {}", "Jos\u{e9} 15557778888", &STANDARD, true);
    assert_as_in_clear("fakey", "X-Tok: {}", "\u{c9}lodie +15557778888", &STANDARD_NO_PAD, true);
}

#[test]
fn a_base64_run_glued_to_a_token_by_an_alphabet_character_is_read() {
    let number = "+15557778888";
    for place in ["X-Url: <https://api.example.com/v1/{}>", "X-Tok: id-{}", "X-Tok: {}-1"] {
        assert_as_in_clear("fakey", place, number, &STANDARD_NO_PAD, true);
    }
}

#[test]
fn the_addresses_and_hosts_of_a_decoded_text_are_mapped_and_read_by_the_leak_check() {
    let mapped = "cli=+15557778888;ip=85.12.34.10;peer=sip:fake-pbx.notreal-carrier.de";
    assert_as_in_clear("fakey", "X-Tok: {}", mapped, &STANDARD, true);
    let (decoded, ..) = in_clear_and_encoded("fakey", "X-Tok: {}", mapped, &STANDARD);
    assert!(
        !decoded.contains("85.12.34.10") && !decoded.contains("notreal-carrier"),
        "the address and the host are mapped: {decoded}"
    );
    let named = "cli=+15557778888;ip=85.12.34.10;name=zorro.nobody";
    let (decoded, clear, verdict, clear_verdict) =
        in_clear_and_encoded("fakey", "X-Tok: {}", named, &STANDARD);
    assert_eq!(decoded, clear);
    assert!(!decoded.contains("85.12.34.10") && !decoded.contains("zorro.nobody"), "{decoded}");
    assert_eq!((verdict, clear_verdict), (0, 0), "nothing is left to count");
}

/// A document left as found is judged alike with a text in clear and with
/// the same text encoded: the leak check reads the decoded text.
#[test]
fn the_leak_check_counts_in_a_decoded_text_what_it_counts_in_the_clear_text() {
    let unexplained = |raw: &Flows| {
        crate::verify::leaks::unexplained(
            raw,
            &Scope::default(),
            &crate::anonymize::Fakes::default(),
            &crate::testflow::reading(),
        )
    };
    let text = "cli=+15557778888;ip=85.12.34.10;name=zorro.nobody";
    let clear = unexplained(&doc("fakey", &[format!("X-Tok: {text}")]));
    let encoded = unexplained(&doc("fakey", &[format!("X-Tok: {}", STANDARD.encode(text))]));
    for token in [("digits", "15557778888"), ("address", "85.12.34.10"), ("domain", "zorro.nobody")]
    {
        let token = (token.0, token.1.as_bytes().to_vec());
        assert!(clear.contains(&token), "{token:?} is counted in clear");
    }
    assert_eq!(encoded, clear);
}

/// A `from-tag` value is the identifier pass's wherever it stands; an `m=`
/// line is a counter in a description, a number in a header value.
#[test]
fn a_decoded_text_takes_the_exemptions_of_the_place_it_stands_at() {
    let tagged = "from-tag=4455667788;cli=+15557778888";
    assert_as_in_clear("fakey", "X-Dialog: {}", tagged, &STANDARD, true);
    assert_as_in_clear("fakey", "X-Media: {}", "m=audio 4455667 RTP/AVP 0", &STANDARD, true);
    for (place, text) in [("X-Dialog: {}", tagged), ("X-Media: {}", "m=audio 4455667 RTP/AVP 0")] {
        let (_, _, verdict, _) = in_clear_and_encoded("fakey", place, text, &STANDARD);
        assert_eq!(verdict, 0, "{text}: a valid capture is not refused");
    }
}

#[test]
fn a_decoded_text_adds_no_key_the_clear_text_would_not_add() {
    let token = STANDARD.encode("cli=+15557778888;exp=3600;cause=1234");
    let clear = ["Reason: Q.850;cause=16".to_string(), "X-Code: 1234".to_string()];
    let with_token = [vec![format!("X-Tok: {token}")], clear.to_vec()].concat();
    let (out, verdict, report) = anonymised_with_report(&doc("fakey", &with_token));
    let (_, _, without) = anonymised_with_report(&doc("fakey", &clear));
    assert!(wire(&out).contains("X-Code: 1234\r\n"), "a short run in clear stays: {}", wire(&out));
    assert_eq!(
        report.entries.len(),
        without.entries.len() + 1,
        "the token adds its one number: {:?}",
        report.entries
    );
    assert_eq!(verdict, 0);
}

#[test]
fn a_base64_user_to_user_payload_is_mapped_as_a_text_one_is() {
    let text = "cli=+15557778888";
    let encoded = STANDARD.encode(text);
    let (clear, clear_verdict) =
        anonymised(&doc("fakey", &[format!("User-to-User: {text};encoding=ascii")]));
    let (out, verdict, report) = anonymised_with_report(&doc(
        "fakey",
        &[format!("User-to-User: {encoded};encoding=base64")],
    ));
    let written = between(&wire(&out), "User-to-User: ", &[';']);
    let decoded = STANDARD.decode(&written).expect("the payload is still base64");
    assert_eq!(
        String::from_utf8(decoded).expect("text"),
        between(&wire(&clear), "User-to-User: ", &[';']),
        "the payload decodes to what the text payload became"
    );
    assert_ne!(written, encoded, "the number is mapped");
    assert_eq!(report.uui_skipped, 0, "a base64 payload that is text is read, not skipped");
    assert_eq!((verdict, clear_verdict), (0, 0));
}

#[test]
fn a_copy_of_a_call_id_that_is_base64_text_takes_the_call_id_s_fake() {
    let call = STANDARD_NO_PAD.encode("sess-1555634567-77");
    let raw = doc_of("fakey", &call, &[format!("X-Orig-Call-ID: {call}")], None);
    let (out, verdict) = anonymised(&raw);
    let wire = wire(&out);
    let local = between(&wire, "Call-ID: ", &['@']);
    assert_ne!(local, call, "the Call-ID is mapped");
    assert_eq!(between(&wire, "X-Orig-Call-ID: ", &['\r']), local, "the copy follows it");
    assert_eq!(verdict, 0);
}

/// A sipfrag whose `X-Tok` header carries `token`.
fn sipfrag(token: &str) -> Flows {
    let body = format!("SIP/2.0 200 OK\r\nX-Tok: {token}\r\n");
    doc_of("fakey", "enc-call", &[], Some(("message/sipfrag", &body)))
}

#[test]
fn the_leak_check_reads_a_base64_run_of_a_body_the_harvest_reads() {
    let text = "cli=+15557778888";
    let raw = sipfrag(&STANDARD.encode(text));
    let left = crate::verify::leaks::unexplained(
        &raw,
        &Scope::default(),
        &crate::anonymize::Fakes::default(),
        &crate::testflow::reading(),
    );
    assert!(
        left.contains(&("digits", b"15557778888".to_vec())),
        "the number left in the body is read decoded"
    );
    let (out, verdict) = anonymised(&raw);
    let (clear, clear_verdict) = anonymised(&sipfrag(text));
    let decoded = STANDARD.decode(between(&wire(&out), "X-Tok: ", &['\r'])).expect("base64");
    assert_eq!(
        String::from_utf8(decoded).expect("text"),
        between(&wire(&clear), "X-Tok: ", &['\r']),
        "the run of the body is rewritten as the clear text is"
    );
    assert_eq!((verdict, clear_verdict), (0, 0));
}

/// A text random bytes may spell — under twenty bytes, without a number's
/// digits — adds no key of its own and takes the keys the rest of the
/// capture gives.
#[test]
fn a_text_short_of_sure_adds_no_key_and_takes_the_keys_of_the_capture() {
    let text = "sip:zorro@pbx7.de";
    let encoded = STANDARD_NO_PAD.encode(text);
    let (alone, verdict) = anonymised(&doc("fakey", &[format!("X-Tok: {encoded}")]));
    assert_eq!(between(&wire(&alone), "X-Tok: ", &['\r']), encoded, "no key of its own");
    assert_eq!(verdict, 0, "judged as written");
    let lines = [format!("X-Tok: {encoded}"), format!("X-Peer: <{text}>")];
    let (keyed, verdict) = anonymised(&doc("fakey", &lines));
    let wire = wire(&keyed);
    let decoded = STANDARD_NO_PAD.decode(between(&wire, "X-Tok: ", &['\r'])).expect("base64");
    let decoded = String::from_utf8(decoded).expect("text");
    assert_ne!(decoded, text, "the keys of the capture map it");
    assert_eq!(format!("<{decoded}>"), between(&wire, "X-Peer: ", &['\r']));
    assert_eq!(verdict, 0);
}

/// A run that decodes to text by accident is what it is written as: a name
/// where a name stands, a token left as found where nothing maps it.
#[test]
fn a_run_that_decodes_to_text_by_accident_is_read_as_written() {
    let name = STANDARD.encode("fakey!");
    let (out, verdict) = anonymised(&doc(&name, &[]));
    let user = between(&wire(&out), "From: <sip:", &['@']);
    assert_ne!(user, name, "a user name is faked as a name");
    assert_eq!(verdict, 0);
    let token = STANDARD.encode("\u{e9}!\u{e9}!");
    let (out, verdict) = anonymised(&doc("fakey", &[format!("X-Nonce: {token}")]));
    assert_eq!(between(&wire(&out), "X-Nonce: ", &['\r']), token, "its bytes add no key");
    assert_eq!(verdict, 0);
}

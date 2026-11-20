use std::collections::HashSet;

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};

use super::*;
use crate::identities::BodyTypes;

fn text_of(run: &str) -> Option<String> {
    Text::of(run).map(|t| String::from_utf8(t.text).expect("text is UTF-8"))
}

fn found(bytes: &str) -> Vec<&str> {
    runs(bytes.as_bytes()).into_iter().map(|(r, _)| &bytes[r]).collect()
}

#[test]
fn a_run_decodes_to_text_only_as_canonical_base64_of_utf8_without_a_control_character() {
    assert_eq!(text_of("KzE1NTU2MzQ1Njc4").as_deref(), Some("+15556345678"));
    assert_eq!(text_of("MTU1NTYzNDU2Nzg").as_deref(), Some("15556345678"), "unpadded");
    assert_eq!(text_of("MTU1NTYzNDU2Nzg=").as_deref(), Some("15556345678"), "padded");
    assert_eq!(text_of("MTU1NTYzNDU2Nzg=="), None, "padding over the block");
    assert_eq!(text_of("MTU1NTYzNDU2Nzh="), None, "trailing bits set: no canonical encoding");
    assert_eq!(text_of(&STANDARD.encode("Jos\u{e9} 1555")).as_deref(), Some("Jos\u{e9} 1555"));
    assert_eq!(text_of(&STANDARD.encode("session-abc-xyz")).as_deref(), Some("session-abc-xyz"));
    assert_eq!(text_of(&STANDARD.encode(b"ab\x01cd+15556345678")), None, "a control byte");
    assert_eq!(text_of(&STANDARD.encode("line one\r\nline two")), None, "a line break");
    assert_eq!(text_of(&STANDARD.encode("tab\there")), None, "a tab");
    assert_eq!(text_of(&STANDARD.encode(b"\xc2\x85 next line")), None, "a C1 control");
    assert_eq!(text_of(&STANDARD.encode(b"\xff\xfe 15556345678")), None, "no UTF-8");
    assert_eq!(text_of("MTIzNDU"), None, "shorter than the shortest run");
    assert_eq!(text_of("MTIzNA==").as_deref(), Some("1234"), "the padding counts in the width");
    assert_eq!(text_of("Pz8-MTU1NTYzNDU2Nzg/"), None, "two alphabets");
    assert_eq!(text_of("155563456781"), None, "digits alone are digits as written");
}

#[test]
fn a_run_of_hex_digits_alone_is_no_base64() {
    let id = "a0c0a0c0";
    let bytes = STANDARD_NO_PAD.decode(id).expect("hex digits are base64 characters");
    assert_eq!(bytes, b"kG4kG4", "an id whose bytes read as text");
    assert_eq!(text_of(id), None);
    assert_eq!(text_of("a0c0a0cg").as_deref(), Some("kG4kG "), "with a letter past `f`");
}

#[test]
fn a_run_is_bounded_by_what_no_alphabet_holds_and_carries_its_padding() {
    let caller = STANDARD.encode("caller=+15557778888");
    assert!(caller.ends_with('='));
    let text = format!("<sip:ref@h;ident=KzE1NTU2MzQ1Njc4>;x={caller},y=!!");
    assert_eq!(found(&text), ["KzE1NTU2MzQ1Njc4", caller.as_str()]);
}

#[test]
fn a_run_glued_to_a_neighbour_is_read_by_its_longest_stretch_that_decodes() {
    let number = STANDARD_NO_PAD.encode("+15557778888");
    let slashed = STANDARD_NO_PAD.encode("ab?c=+15557778888");
    assert!(slashed.contains('/'), "{slashed}");
    assert_eq!(found(&format!("v1/{number}")), [number.as_str()]);
    assert_eq!(found(&format!("id-{number}")), [number.as_str()]);
    assert_eq!(found(&format!("{number}-1")), [number.as_str()]);
    assert_eq!(found(&format!("id-{number}_x")), [number.as_str()]);
    assert_eq!(found(&format!("v1/{slashed}")), [slashed.as_str()], "the stretch keeps its `/`");
    assert_eq!(found(&format!("{number}-{number}")), [number.as_str(), number.as_str()]);
    let pieces = vec!["ab"; MAX_PIECES].join("-");
    assert!(found(&format!("{pieces}-{number}")).is_empty(), "past the bound: whole or nothing");
    let pieces = vec!["ab"; MAX_PIECES - 2].join("-");
    assert_eq!(found(&format!("{pieces}-{number}")), [number.as_str()], "within the bound");
}

#[test]
fn a_text_is_written_back_in_its_alphabet_with_its_padding() {
    for run in ["Pz8-KzE1NTU2MzQ1Njc4", "Pz8_KzE1NTU2MzQ1Njc4", "KzE1NTU2MzQ1Njc4Pw=="] {
        let text = Text::of(run).expect("text");
        assert_eq!(text.encode(&text.text), run, "as written");
        let swapped =
            String::from_utf8(text.text.clone()).unwrap().replace("5556345678", "0009123456");
        let out = text.encode(swapped.as_bytes());
        assert_eq!(out.len(), run.len(), "{run}");
        assert_eq!(text_of(&out).as_deref(), Some(swapped.as_str()), "{run} -> {out}");
    }
    let plain = Text::of(&URL_SAFE_NO_PAD.encode("?>?>?>?>?")).expect("text");
    assert!(plain.encode(b"?>?>?>?>?").contains('_'), "the URL-safe alphabet is kept");
}

#[test]
fn text_is_beyond_accident_at_its_width_or_with_a_number_s_digits() {
    let sure = |text: &str| Text::of(&STANDARD.encode(text)).expect("text").sure();
    assert!(sure("+15556345678"));
    assert!(sure("+1 555 634 5678"), "a number however it is spelt");
    assert!(sure("fakey.testo@fake-pbx"), "twenty bytes");
    assert!(!sure("fakey.testo@fake-pb"), "nineteen bytes without a number");
    assert!(!sure("ext=54321"), "five digits");
}

fn head(lines: &[&str]) -> String {
    let lines: String = lines.iter().map(|line| format!("{line}\r\n")).collect();
    format!(
        "BYE sip:testo@fake-sbc.example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bKenc1\r\n\
         From: <sip:fakey@fake-sbc.example.com>;tag=encf\r\n\
         To: <sip:testo@fake-sbc.example.com>;tag=enct\r\n\
         Call-ID: enc-call@fake-sbc.example.com\r\nCSeq: 2 BYE\r\nMax-Forwards: 70\r\n{lines}"
    )
}

fn site_texts(raw: &str, identifiers: &[&str]) -> Vec<(String, bool)> {
    let types = BodyTypes::default();
    let identifiers: HashSet<Vec<u8>> = identifiers.iter().map(|w| w.as_bytes().to_vec()).collect();
    sites(raw.as_bytes(), &Context::new(identifiers, &types))
        .into_iter()
        .map(|site| (String::from_utf8(site.text.text).expect("text"), site.sure))
        .collect()
}

#[test]
fn a_site_stands_in_a_value_outside_the_passport_the_identifiers_and_protocol_text() {
    let number = STANDARD.encode("+15557778888");
    let short = STANDARD.encode("fakey.testo");
    let claims = URL_SAFE_NO_PAD.encode(r#"{"orig":{"tn":"15557778888"}}"#);
    let raw = head(&[
        &format!("X-Tok: {number}"),
        &format!("{number}: named"),
        &format!("Identity: {claims}.{claims}.{claims};info=<https://fake-cert.example.com/c>"),
        &format!("X-Orig-Call-ID: {short}"),
        &format!("Event: dialog;call-id={number}"),
        &format!("Reason: Q.850;cause=16;text=\"{number}\""),
        &format!("User-to-User: {};encoding=base64", STANDARD.encode("zorro.nobody")),
        "Content-Length: 0\r\n",
    ]);
    assert_eq!(
        site_texts(&raw, &[]),
        [
            ("+15557778888".to_string(), true),
            ("fakey.testo".to_string(), false),
            ("zorro.nobody".to_string(), true)
        ],
        "the value, the copy while no identifier spells it, the payload stated base64"
    );
    let word = short.trim_end_matches('=');
    assert_eq!(site_texts(&raw, &[word]).len(), 2, "a copy of an identifier word");
}

#[test]
fn a_site_of_a_body_is_read_and_the_reading_states_its_own_length() {
    let token = STANDARD.encode("cli=+15557778888");
    let body = format!("SIP/2.0 200 OK\r\nX-Tok: {token}\r\n");
    let length = format!("Content-Length: {}\r\n", body.len());
    let raw = head(&["Content-Type: message/sipfrag", &length]) + &body;
    let types = BodyTypes::default();
    let reading = read(raw.as_bytes(), &Context::new(HashSet::new(), &types)).expect("a site");
    let read = String::from_utf8(reading.bytes.clone()).expect("text");
    let decoded = "SIP/2.0 200 OK\r\nX-Tok: cli=+15557778888\r\n";
    assert!(read.ends_with(decoded), "{read}");
    assert!(read.contains(&format!("Content-Length: {}\r\n", decoded.len())), "{read}");
    assert_eq!(read[reading.spans[0].clone()], *"cli=+15557778888");
    assert_eq!(raw[reading.runs[0].clone()], token);
    assert!(crate::testflow::reading().parse(&reading.bytes).is_ok(), "the reading parses");
    assert_eq!(reading.alike(raw.as_bytes()).as_deref(), Some(reading.bytes.as_slice()));
    assert_eq!(reading.alike(raw.replace(&token, "!!!!").as_bytes()), None);
}

#[test]
fn a_part_the_anonymiser_does_not_read_holds_no_site() {
    let token = STANDARD.encode("cli=+15557778888");
    let body = format!("ref {token}\r\n");
    let length = format!("Content-Length: {}\r\n", body.len());
    let raw = head(&["Content-Type: application/vnd.example.order", &length]) + &body;
    assert!(site_texts(&raw, &[]).is_empty());
}

/// A pass that maps the number `5557778888` and the key `05`, and fakes the
/// word `fakey`.
fn pass(bytes: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(bytes)
        .replace("5557778888", "0009123456")
        .replace("fakey", "zzzzz")
        .replace("05", "99")
        .into_bytes()
}

#[test]
fn the_decoded_reading_writes_a_site_the_pass_leaves_as_written() {
    let types = BodyTypes::default();
    let cx = Context::new(HashSet::new(), &types);
    let token = STANDARD.encode("cli=+15557778888");
    let raw = head(&[&format!("X-Tok: {token}"), "Content-Length: 0\r\n"]);
    let out = String::from_utf8(rewrite(raw.as_bytes(), &cx, pass)).expect("text");
    assert!(out.contains(&format!("X-Tok: {}\r\n", STANDARD.encode("cli=+10009123456"))), "{out}");
    assert!(out.contains("sip:zzzzz@"), "the pass over the datagram as written stands: {out}");
}

/// The encoding of `fakey!` is a name as written, which the pass rewrites;
/// the encoding of the second text spells `05`, a key, by accident.
#[test]
fn a_run_the_pass_rewrites_as_written_is_its_own_but_for_digits_spelt_by_accident() {
    let types = BodyTypes::default();
    let cx = Context::new(HashSet::new(), &types);
    let named = STANDARD.encode("fakey!");
    let own =
        |bytes: &[u8]| String::from_utf8_lossy(bytes).replace(&named, "Qrstuvwx").into_bytes();
    let raw = head(&[&format!("X-Tok: {named}"), "Content-Length: 0\r\n"]);
    let out = String::from_utf8(rewrite(raw.as_bytes(), &cx, |b| pass(&own(b)))).expect("text");
    assert!(out.contains("X-Tok: Qrstuvwx\r\n"), "the run rewritten as written stands: {out}");
    let text = "x=9;cli=+15557778888";
    let token = STANDARD.encode(text);
    assert!(token.contains("05"), "{token}");
    let raw = head(&[&format!("X-Tok: {token}"), "Content-Length: 0\r\n"]);
    let out = String::from_utf8(rewrite(raw.as_bytes(), &cx, pass)).expect("text");
    let mapped = STANDARD.encode(text.replace("5557778888", "0009123456"));
    assert!(out.contains(&format!("X-Tok: {mapped}\r\n")), "the decoded reading stands: {out}");
}

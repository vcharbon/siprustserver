use super::*;

#[test]
fn a_token_no_fake_explains_is_reported_by_site_and_shape_never_by_value() {
    let output = vec![
        b"INVITE sip:x@h SIP/2.0\r\nOrganization: Fakenetco Ltd\r\nCall-ID: c\r\n\r\n".to_vec(),
        b"SIP/2.0 200 OK\r\nOrganization: Fakenetco Ltd\r\n\r\n".to_vec(),
    ];
    let found = finding("word", b"fakenetco", &output);
    assert_eq!(
        found,
        Finding {
            class: "word".into(),
            site: "Organization".into(),
            shape: "aaaaaaaaa".into(),
            datagrams: 2
        }
    );
    assert!(!format!("{found:?}").to_ascii_lowercase().contains("fakenetco"));
}

#[test]
fn a_site_is_the_start_line_a_header_folded_or_not_or_the_body() {
    let raw = b"INVITE sip:a@h SIP/2.0\r\nSubject: one\r\n two\r\nCall-ID: c\r\n\r\nv=0\r\n";
    let at = |needle: &[u8]| raw.windows(needle.len()).position(|w| w == needle).expect("present");
    assert_eq!(identities::site_of(raw, at(b"sip:a")), "start-line");
    assert_eq!(identities::site_of(raw, at(b"two")), "Subject");
    assert_eq!(identities::site_of(raw, at(b": c")), "Call-ID");
    assert_eq!(identities::site_of(raw, at(b"v=0")), "body");
    assert_eq!(shape("Ab-9 é"), "Aa-9 ??");
}

#[test]
fn a_digit_run_is_reported_where_it_stands_whole_outside_a_counter() {
    let output = vec![
        b"INVITE sip:x@h SIP/2.0\r\nCSeq: 1234567 INVITE\r\nX-B: 91234567\r\nX-A: 1234567\r\n\r\n"
            .to_vec(),
    ];
    assert_eq!(finding("digits", b"1234567", &output).site, "X-A");
}

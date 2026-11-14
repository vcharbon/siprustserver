use super::*;

fn texts(input: &str) -> Vec<&str> {
    literals(input.as_bytes()).into_iter().map(|(s, e)| &input[s..e]).collect()
}

#[test]
fn an_address_is_found_wherever_sip_and_sdp_write_one() {
    let datagram = "Via: SIP/2.0/UDP 1.1.1.3:5060;received=1.1.1.4\r\n\
                    Contact: <sip:+2861@[3fff:cb0::5]:5060>\r\n\r\n\
                    o=- 1 1 IN IP4 1.1.1.3\r\nc=IN IP6 3fff:cb0::7\r\n";
    assert_eq!(texts(datagram), ["1.1.1.3", "1.1.1.4", "3fff:cb0::5", "1.1.1.3", "3fff:cb0::7"]);
    assert_eq!(texts("1.1.1.3:5060"), ["1.1.1.3"]);
    assert_eq!(texts("Route: <sip:1.1.1.9:5060;lr>"), ["1.1.1.9"]);
}

#[test]
fn an_unbracketed_or_mapped_ipv6_address_is_found_too() {
    assert_eq!(texts("Via: SIP/2.0/UDP h.zz;received=3fff:e0a:1:2::7;rport"), ["3fff:e0a:1:2::7"]);
    assert_eq!(texts("Via: SIP/2.0/UDP h.zz;maddr=3fff:6d0::9"), ["3fff:6d0::9"]);
    assert_eq!(
        texts("a=candidate:1 1 UDP 2130706431 3fff:e0a:1:2::8 9000 typ host"),
        ["3fff:e0a:1:2::8"]
    );
    assert_eq!(texts("X-Peer-V6: 3fff:e0a:1:2::7\r\n"), ["3fff:e0a:1:2::7"]);
    assert_eq!(texts("X-Origin: [::ffff:8.8.8.202]"), ["::ffff:8.8.8.202"]);
}

#[test]
fn a_version_a_date_a_time_or_a_word_is_no_address() {
    assert!(texts("Vendor/7.2.1 build 1.2.3.4.5").is_empty());
    assert!(texts("v10.0.2.4").is_empty());
    assert!(texts("10.0.2.4x").is_empty());
    assert!(texts("2026.10.12").is_empty());
    assert!(texts("[not:an:address]").is_empty());
    assert!(texts("Date: Sat, 13 Nov 2010 23:29:00 GMT").is_empty());
    assert!(texts("a=fingerprint:sha-256 AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB")
        .is_empty());
    assert!(texts("Call-ID: deadbeef@h").is_empty());
}

#[test]
fn the_mapping_decides_each_literal() {
    let out =
        rewrite(b"a 1.1.1.3 b 10.0.0.1 c", &|t| (t == "1.1.1.3").then(|| "0.11.22.3".to_string()));
    assert_eq!(out, b"a 0.11.22.3 b 10.0.0.1 c");
}

#[test]
fn an_address_glued_into_an_id_behind_a_letter_and_a_dot_is_found() {
    assert_eq!(texts("branch=z9hG4bK.192.0.2.71;rport"), ["192.0.2.71"]);
    assert_eq!(texts("x-192.0.2.73-77ab@h"), ["192.0.2.73"]);
    assert!(texts("v1.2.3.4.5").is_empty());
}

#[test]
fn an_address_behind_a_percent_escape_is_found_and_the_escape_is_not_one() {
    assert_eq!(texts("sip:88666%40192.0.2.51%3Btransport"), ["192.0.2.51"]);
}

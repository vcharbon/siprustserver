use super::via_response_target;

/// RFC 3261 §7.3.1: an originator may fold its hops onto one Via line, and the
/// snapshot keeps the line as written; the response goes to the first hop.
#[test]
fn a_folded_via_line_answers_its_first_hop() {
    let line = "SIP/2.0/UDP 192.0.2.10:5070;branch=z9hG4bK-a, SIP/2.0/UDP 192.0.2.20:5080;branch=z9hG4bK-b";
    assert_eq!(via_response_target(line), Some(("192.0.2.10".to_string(), 5070)));
}

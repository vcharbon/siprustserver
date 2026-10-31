use crate::{CustomParser, SipMessage, SipParser};

fn request_with_vias(vias: &[&str]) -> SipMessage {
    let via: String = vias.iter().map(|v| format!("Via: SIP/2.0/UDP {v}\r\n")).collect();
    let raw = format!(
        "OPTIONS sip:bob@example.test SIP/2.0\r\n{via}Max-Forwards: 70\r\n\
         From: <sip:alice@example.test>;tag=a\r\nTo: <sip:bob@example.test>\r\n\
         Call-ID: vias\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n"
    );
    CustomParser::new().parse(raw.as_bytes()).expect("parse should succeed")
}

/// The lower Via hops are held at their count: a single-Via message holds no
/// room for them at all.
#[test]
fn the_lower_via_hops_are_held_at_their_count() {
    let hops = [
        "10.0.0.1:5060;branch=z9hG4bK-a",
        "10.0.0.2:5060;branch=z9hG4bK-b",
        "10.0.0.3:5060;branch=z9hG4bK-c",
    ];
    for n in 1..=hops.len() {
        let msg = request_with_vias(&hops[..n]);
        assert_eq!((msg.via().len(), msg.via().tail_capacity()), (n, n - 1));
    }
}

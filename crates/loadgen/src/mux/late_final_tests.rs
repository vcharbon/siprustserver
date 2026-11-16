use super::*;

const PEER: &str = "127.0.0.1:5080";

fn invite(cseq: u32) -> Vec<u8> {
    format!(
        "INVITE sip:b@127.0.0.1 SIP/2.0\r\nMax-Forwards: 70\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}\r\n\
         Contact: <sip:a@127.0.0.1:5060>\r\nCall-ID: lf1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\n\
         CSeq: {cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn final_of(status: u16, cseq: u32) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}\r\n\
         Contact: <sip:b@127.0.0.1:5080>\r\nCall-ID: lf1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\n\
         CSeq: {cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A re-INVITE refused 491 and retried: once the retry is answered and the
/// call released, a retransmission of the 491 still draws its hop ACK
/// (RFC 3261 §17.1.1.2), not a stray.
#[test]
fn an_earlier_invite_s_non_2xx_keeps_its_ack_across_a_later_answered_invite() {
    let invites = SentInvites::default();
    let dst: SocketAddr = PEER.parse().unwrap();
    invites.on_outbound(&Datagram::sniff(&invite(3)), dst);
    invites.on_inbound(&Datagram::sniff(&final_of(491, 3)));
    invites.on_outbound(&Datagram::sniff(&invite(4)), dst);
    invites.on_inbound(&Datagram::sniff(&final_of(200, 4)));
    let mut held = invites.take("lf1@h").expect("the earlier ACK is still held");
    let Reply::AckNon2xx(ack, to) = held.on_final(&Datagram::sniff(&final_of(491, 3))) else {
        panic!("the retransmitted 491 draws its ACK");
    };
    let id = Datagram::sniff(&ack).id().clone();
    assert_eq!((id.method, id.cseq, to), (Some("ACK"), Some(3), dst));
}

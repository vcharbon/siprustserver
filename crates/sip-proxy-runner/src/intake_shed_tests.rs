//! Pins [`intake_shed_hook`]: the hook is pure over `(raw, depth)`
//! — no socket needed — so each case invokes it directly.

use super::*;

const WATERMARK: usize = 8;

fn src() -> SocketAddr {
    "192.0.2.9:5060".parse().unwrap()
}

const NEW_INVITE: &[u8] = b"INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-shed\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";

const IN_DIALOG_INVITE: &[u8] = b"INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-re\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b2\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 2 INVITE\r\nContent-Length: 0\r\n\r\n";

const EMERGENCY_INVITE: &[u8] = b"INVITE sip:911@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-em\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:911@example.com>\r\n\
Resource-Priority: esnet.0\r\n\
Call-ID: em@10.0.0.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";

const BYE: &[u8] = b"BYE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-bye\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b2\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 3 BYE\r\nContent-Length: 0\r\n\r\n";

const OK_200: &[u8] = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-shed\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b2\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";

#[test]
fn below_the_watermark_everything_is_admitted() {
    let hook = intake_shed_hook(WATERMARK);
    for raw in [NEW_INVITE, IN_DIALOG_INVITE, EMERGENCY_INVITE, BYE, OK_200] {
        assert_eq!(hook(raw, src(), WATERMARK - 1), PreIngressAction::Accept);
    }
}

#[test]
fn at_the_watermark_only_the_new_non_emergency_invite_is_dropped() {
    let hook = intake_shed_hook(WATERMARK);
    assert_eq!(hook(NEW_INVITE, src(), WATERMARK), PreIngressAction::Drop);
    for admitted in [IN_DIALOG_INVITE, EMERGENCY_INVITE, BYE, OK_200] {
        assert_eq!(hook(admitted, src(), WATERMARK), PreIngressAction::Accept);
    }
}

#[test]
fn deeper_than_the_watermark_still_sheds_new_invites() {
    let hook = intake_shed_hook(WATERMARK);
    assert_eq!(hook(NEW_INVITE, src(), WATERMARK * 100), PreIngressAction::Drop);
}

#[test]
fn watermark_is_the_percentage_of_the_queue_and_floors_at_one() {
    assert_eq!(intake_shed_watermark(8192, 50), 4096);
    assert_eq!(intake_shed_watermark(1, 50), 1);
    assert_eq!(intake_shed_watermark(8192, 0), 1);
}

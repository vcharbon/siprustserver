use std::net::SocketAddr;

use layer_harness::Stamped;
use sip_net::{RecvDisposition, UdpPacket};
use sip_net_testkit::{SignalingNetworkEvent, WireStamp};

use super::arrived_before;

const PARTY: &str = "127.0.0.1:5060";
const PEER: &str = "127.0.0.1:5080";

fn addr(a: &str) -> SocketAddr {
    a.parse().unwrap()
}

fn response(status: u16, method: &str) -> Vec<u8> {
    format!("SIP/2.0 {status} X\r\nCSeq: 1 {method}\r\nCall-ID: c\r\n\r\n").into_bytes()
}

fn stamped(events: Vec<SignalingNetworkEvent>) -> Vec<Stamped<SignalingNetworkEvent>> {
    let at = |(seq, event)| Stamped { event, seq, at_ms: seq };
    (0..).zip(events).map(at).collect()
}

/// The peer's send of `msg` to the party.
fn sent(msg: &[u8]) -> SignalingNetworkEvent {
    SignalingNetworkEvent::SendCalled {
        bind_key: PEER.to_string(),
        to: addr(PARTY),
        msg: msg.into(),
    }
}

/// The loss of the peer's send of `msg` to the party.
fn lost(msg: &[u8]) -> SignalingNetworkEvent {
    SignalingNetworkEvent::SendLost { bind_key: PEER.to_string(), to: addr(PARTY), msg: msg.into() }
}

/// The delivery of `msg` from the peer to the party.
fn delivered(msg: &[u8]) -> SignalingNetworkEvent {
    SignalingNetworkEvent::RecvItem {
        bind_key: PARTY.to_string(),
        packet: UdpPacket::unparsed(msg.to_vec(), addr(PEER), 0),
        disposition: RecvDisposition::Delivered,
        wire: WireStamp::of_bytes(msg),
    }
}

/// A final to the CANCEL the peer sent before its 2xx, still in flight when
/// the 2xx's first copy is lost, reaches the party ahead of it: the 2xx was
/// not sent before the CANCEL was settled.
#[test]
fn a_response_sent_before_a_lost_copy_and_in_flight_arrived_before_it() {
    let (ring, cancel_ok, answer) =
        (response(180, "INVITE"), response(200, "CANCEL"), response(200, "INVITE"));
    let events = stamped(vec![
        sent(&ring),
        delivered(&ring),
        sent(&cancel_ok),
        sent(&answer),
        lost(&answer),
    ]);
    let got = arrived_before(&events, &lost(&answer), addr(PARTY), true);
    assert_eq!(got, vec![(180, "INVITE".to_string()), (200, "CANCEL".to_string())]);
}

/// A response sent after the lost copy, and the lost copy itself, reached
/// the party no earlier than it; one whose own send was lost reached it only
/// once delivered.
#[test]
fn a_response_sent_after_a_lost_copy_or_itself_lost_did_not_arrive_before_it() {
    let (ring, answer, cancel_ok) =
        (response(183, "INVITE"), response(200, "INVITE"), response(200, "CANCEL"));
    let events = stamped(vec![
        sent(&ring),
        lost(&ring),
        sent(&answer),
        lost(&answer),
        sent(&cancel_ok),
        delivered(&cancel_ok),
    ]);
    assert_eq!(arrived_before(&events, &lost(&answer), addr(PARTY), true), vec![]);
}

/// A datagram the party sent and the fabric lost is ordered against what
/// was delivered to it, never against what is still in flight to it.
#[test]
fn a_lost_send_of_the_party_reads_only_what_was_delivered_to_it() {
    let (ring, cancel_ok) = (response(180, "INVITE"), response(200, "CANCEL"));
    let ack = b"ACK sip:x SIP/2.0\r\nCSeq: 1 ACK\r\n\r\n";
    let party_lost = SignalingNetworkEvent::SendLost {
        bind_key: PARTY.to_string(),
        to: addr(PEER),
        msg: ack.to_vec(),
    };
    let events = stamped(vec![
        sent(&ring),
        delivered(&ring),
        sent(&cancel_ok),
        SignalingNetworkEvent::SendCalled {
            bind_key: PARTY.to_string(),
            to: addr(PEER),
            msg: ack.to_vec(),
        },
        party_lost.clone(),
    ]);
    let got = arrived_before(&events, &party_lost, addr(PARTY), false);
    assert_eq!(got, vec![(180, "INVITE".to_string())]);
}

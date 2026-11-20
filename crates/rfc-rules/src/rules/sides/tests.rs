//! Which socket each side of a transaction is filed under.

use crate::wire::{Kind, Msg};

use super::Sides;

/// The UAC's listening socket, its Via sent-by.
const UAC_LISTENS: &str = "10.0.0.1:5060";
/// The ephemeral socket the UAC sends from.
const UAC_SENDS: &str = "10.0.0.1:40000";
/// The UAS's listening socket.
const UAS_LISTENS: &str = "10.0.0.2:5060";
/// The socket the UAS answers from.
const UAS_SENDS: &str = "10.0.0.2:41000";

fn msg(src: &str, dst: &str, kind: Kind, method: &str, branch: &str) -> Msg {
    let head = format!(
        "Via: SIP/2.0/UDP {UAC_LISTENS};branch={branch}\r\n\
         Call-ID: c1\r\nCSeq: 1 {method}\r\n\r\n"
    );
    Msg {
        refused: false,
        at_us: 0,
        src: src.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind,
        call_id: "c1".to_string(),
        cseq: 1,
        cseq_method: method.to_string(),
        via_branch: Some(branch.to_string()),
        from_tag: Some("ft".to_string()),
        to_tag: None,
        head: Some(head.into_bytes()),
        body: None,
    }
}

fn request(src: &str, dst: &str, method: &str, branch: &str) -> Msg {
    msg(src, dst, Kind::Request { method: method.to_string() }, method, branch)
}

fn response(src: &str, dst: &str, status: u16, method: &str, branch: &str) -> Msg {
    msg(src, dst, Kind::Response { status }, method, branch)
}

/// A response reaching the UAC's sent-by is filed under the socket the
/// request left from; one answered from another UAS socket under the socket
/// the request reached.
#[test]
fn a_response_is_filed_under_its_request_s_sockets() {
    let msgs = [
        request(UAC_SENDS, UAS_LISTENS, "INVITE", "z9hG4bK-1"),
        response(UAS_SENDS, UAC_LISTENS, 200, "INVITE", "z9hG4bK-1"),
    ];
    let sides = Sides::of(&msgs);
    assert_eq!((sides.uac(&msgs[0]), sides.uas(&msgs[0])), (UAC_SENDS, UAS_LISTENS));
    assert_eq!((sides.uac(&msgs[1]), sides.uas(&msgs[1])), (UAC_SENDS, UAS_LISTENS));
}

/// A response sent to no address the request's sender wrote reached no UAC:
/// its own destination names that side.
#[test]
fn a_response_past_the_sent_by_keeps_its_destination() {
    let astray = "10.0.0.9:5060";
    let msgs = [
        request(UAC_SENDS, UAS_LISTENS, "INVITE", "z9hG4bK-1"),
        response(UAS_SENDS, astray, 200, "INVITE", "z9hG4bK-1"),
    ];
    let sides = Sides::of(&msgs);
    assert_eq!((sides.uac(&msgs[1]), sides.uas(&msgs[1])), (astray, UAS_LISTENS));
}

/// A CANCEL shares its INVITE's branch but is its own transaction: its
/// response names the CANCEL's sockets, the INVITE's response the INVITE's.
#[test]
fn the_cseq_method_keeps_a_cancel_apart_from_its_invite() {
    let elsewhere = "10.0.0.2:5080";
    let msgs = [
        request(UAC_SENDS, UAS_LISTENS, "INVITE", "z9hG4bK-1"),
        request(UAC_SENDS, elsewhere, "CANCEL", "z9hG4bK-1"),
        response(UAS_SENDS, UAC_LISTENS, 200, "CANCEL", "z9hG4bK-1"),
        response(UAS_SENDS, UAC_LISTENS, 487, "INVITE", "z9hG4bK-1"),
    ];
    let sides = Sides::of(&msgs);
    assert_eq!(sides.uas(&msgs[2]), elsewhere);
    assert_eq!(sides.uas(&msgs[3]), UAS_LISTENS);
}

/// A key carried between two endpoint pairs names no request: each response
/// keeps its own sockets.
#[test]
fn a_key_on_two_pairs_names_no_request() {
    let relay = "10.0.0.3:5060";
    let msgs = [
        request(UAC_SENDS, relay, "INVITE", "z9hG4bK-1"),
        request(relay, UAS_LISTENS, "INVITE", "z9hG4bK-1"),
        response(UAS_SENDS, UAC_LISTENS, 200, "INVITE", "z9hG4bK-1"),
    ];
    let sides = Sides::of(&msgs);
    assert!(sides.request_of(&msgs[2]).is_none());
    assert_eq!((sides.uac(&msgs[2]), sides.uas(&msgs[2])), (UAC_LISTENS, UAS_SENDS));
}

/// A response whose request this view never carried keeps its own sockets.
#[test]
fn an_orphan_response_keeps_its_own_sockets() {
    let msgs = [response(UAS_SENDS, UAC_LISTENS, 200, "INVITE", "z9hG4bK-1")];
    let sides = Sides::of(&msgs);
    assert_eq!((sides.uac(&msgs[0]), sides.uas(&msgs[0])), (UAC_LISTENS, UAS_SENDS));
}

/// A response travelling its request's own way answers nothing.
#[test]
fn a_response_in_the_request_s_direction_names_no_request() {
    let msgs = [
        request(UAC_SENDS, UAS_LISTENS, "INVITE", "z9hG4bK-1"),
        response(UAC_SENDS, UAS_LISTENS, 200, "INVITE", "z9hG4bK-1"),
    ];
    let sides = Sides::of(&msgs);
    assert!(sides.request_of(&msgs[1]).is_none());
}

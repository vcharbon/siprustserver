//! The rule's own semantics under a CLOSED observation: which responses open
//! an occasion, and which address each Via stamp names.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding};
use crate::wire::{Kind, Msg, Observation, WireView};

use super::super::super::Obligation;
use super::ResponseDestination;

const ALICE: &str = "192.0.2.10:5060";
const BOB: &str = "192.0.2.20:5070";

/// Bob's 200 to alice's OPTIONS, sent to `dst` under the top Via `via`.
fn answer(dst: &str, via: &str) -> Msg {
    let head = format!(
        "SIP/2.0 200 OK\r\n\
         Via: {via}\r\n\
         From: <sip:alice@example.com>;tag=at\r\n\
         To: <sip:bob@example.com>;tag=bt\r\n\
         Call-ID: c1\r\n\
         CSeq: 5 OPTIONS\r\n\
         Content-Length: 0\r\n\r\n"
    );
    Msg {
        refused: false,
        at_us: 1_000,
        src: BOB.to_string(),
        dst: dst.to_string(),
        hop: 0,
        repeat: false,
        kind: Kind::Response { status: 200 },
        call_id: "c1".to_string(),
        cseq: 5,
        cseq_method: "OPTIONS".to_string(),
        from_tag: Some("at".to_string()),
        to_tag: Some("bt".to_string()),
        via_branch: Some("z9hG4bK-d".to_string()),
        head: Some(head.into_bytes()),
        body: Some(Vec::new()),
    }
}

fn eval(msgs: &[Msg]) -> Vec<Finding> {
    let obs = Observation { closed: true, endpoint_last_us: BTreeMap::new(), last_us: 1_000 };
    ResponseDestination.eval(&WireView { msgs, obs: &obs })
}

fn decision(dst: &str, via: &str) -> Decision {
    let all = eval(&[answer(dst, via)]);
    assert_eq!(all.len(), 1, "one occasion: {all:?}");
    all.into_iter().next().map(|f| f.decision).unwrap_or(Decision::Compliant)
}

/// The sent-by's address and port, the default port where none is stated,
/// `received` over the sent-by host and an observed `rport` over its port.
#[test]
fn a_response_to_its_via_target_is_compliant() {
    for (dst, via) in [
        (ALICE, "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d"),
        (ALICE, "SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-d"),
        (ALICE, "SIP/2.0/UDP 198.51.100.1:5060;branch=z9hG4bK-d;received=192.0.2.10"),
        ("192.0.2.10:6000", "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d;rport=6000"),
        ("[2001:db8::10]:5060", "SIP/2.0/UDP [2001:db8::10]:5060;branch=z9hG4bK-d"),
        (ALICE, "SIP/2.0/UDP alice.example.com:5060;branch=z9hG4bK-d;received=192.0.2.10"),
        (ALICE, "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d;maddr=203.0.113.9"),
    ] {
        assert!(matches!(decision(dst, via), Decision::Compliant), "{dst} {via}");
    }
}

/// The source port where the Via asked for no `rport`, a sent-by over its
/// `received`, and the sent-by port under an observed `rport`: each misses.
#[test]
fn a_response_elsewhere_is_violated() {
    for (dst, via, target) in [
        ("192.0.2.10:6000", "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d", "192.0.2.10:5060"),
        (
            "198.51.100.1:5060",
            "SIP/2.0/UDP 198.51.100.1:5060;branch=z9hG4bK-d;received=192.0.2.10",
            "192.0.2.10:5060",
        ),
        (ALICE, "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d;rport=6000", "192.0.2.10:6000"),
    ] {
        let Decision::Violated(Evidence::ResponseDestinationDiverged {
            dest_sent_to,
            via_target,
            status,
            response_dest_method,
            ..
        }) = decision(dst, via)
        else {
            panic!("{dst} {via}");
        };
        assert_eq!((dest_sent_to.as_str(), via_target.as_str()), (dst, target));
        assert_eq!((status, response_dest_method.as_str()), (200, "OPTIONS"));
    }
}

/// A sent-by name no `received` overrides needs RFC 3263 §5, and a vantage
/// whose destination is no address names nowhere to compare with.
#[test]
fn a_name_or_an_unaddressed_vantage_is_undecidable() {
    let named = decision(ALICE, "SIP/2.0/UDP alice.example.com:5060;branch=z9hG4bK-d");
    assert!(matches!(named, Decision::Undecidable(_)), "{named:?}");
    let unaddressed = decision("alice", "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d");
    assert!(matches!(unaddressed, Decision::Undecidable(_)), "{unaddressed:?}");
}

/// A reliable transport answers on the request's connection, a repeat to its
/// first copy's destination opens no occasion of its own, and a request is no
/// response.
#[test]
fn a_reliable_transport_a_repeat_and_a_request_open_no_occasion() {
    assert!(eval(&[answer("192.0.2.10:6000", "SIP/2.0/TCP 192.0.2.10:5060;branch=z9hG4bK-d")])
        .is_empty());
    let via = "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d";
    let mut repeat = answer(ALICE, via);
    repeat.repeat = true;
    assert_eq!(eval(&[answer(ALICE, via), repeat]).len(), 1, "the copy to the same place");
    let mut request = answer(ALICE, "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d");
    request.kind = Kind::Request { method: "OPTIONS".to_string() };
    assert!(eval(&[request]).is_empty());
}

/// The sender is charged, toward the party it sent to.
#[test]
fn the_sender_is_charged() {
    let all = eval(&[answer("192.0.2.10:6000", "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d")]);
    assert_eq!((all[0].emitter.as_str(), all[0].taker.as_str()), (BOB, "192.0.2.10:6000"));
}

/// A repeat sent somewhere its first copy was not is judged on its own: a
/// stray copy after a correct one is the miss.
#[test]
fn a_repeat_sent_elsewhere_is_judged() {
    let via = "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-d";
    let mut stray = answer("192.0.2.10:6000", via);
    stray.repeat = true;
    let all = eval(&[answer(ALICE, via), stray]);
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
    assert!(matches!(all[1].decision, Decision::Violated(_)), "{:?}", all[1].decision);
}

/// A Via still asking for `rport` names its sent-by port, and the evidence
/// says the stamp was never written.
#[test]
fn an_unfilled_rport_is_named_on_the_evidence() {
    let unfilled =
        decision("192.0.2.10:6000", "SIP/2.0/UDP 192.0.2.10:5061;rport;branch=z9hG4bK-d");
    let Decision::Violated(Evidence::ResponseDestinationDiverged {
        rport_unfilled,
        via_target,
        ..
    }) = unfilled
    else {
        panic!("{unfilled:?}");
    };
    assert!(rport_unfilled);
    assert_eq!(via_target, "192.0.2.10:5061");
    let stamped =
        decision("192.0.2.10:6000", "SIP/2.0/UDP 192.0.2.10:5061;rport=6001;branch=z9hG4bK-d");
    assert!(
        matches!(
            stamped,
            Decision::Violated(Evidence::ResponseDestinationDiverged { rport_unfilled: false, .. })
        ),
        "{stamped:?}"
    );
}

//! A response for a call no node holds, answered as an unknown dialog would
//! be (ADR-0055 §4) when its via-extension verifies (ADR-0055 §12): a 2xx
//! ACKed and BYEd from its own headers, an offer it makes to an INVITE that
//! carried none answered with every stream rejected; a non-2xx INVITE final
//! ACKed hop by hop at the Request-URI the extension states; nothing for
//! anything else.

use b2bua_core::config::B2buaConfig;
use b2bua_core::effects::{OutboundBody, OutboundSipEffect};
use b2bua_core::stack_identity::{build_call_via, RequestFacts, StackIdentityOpts};
use b2bua_core::unheld_response::answer_unheld_response;
use sip_ids::{SigningKey, TurnIds};
use sip_message::header::{HeaderValue, Rport};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipResponse};

const OFFER: &str = "v=0\r\no=bob 7 7 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

fn key() -> SigningKey {
    SigningKey::new(&[3u8; 32]).expect("a key")
}

fn config() -> B2buaConfig {
    B2buaConfig { cluster_key: Some(key()), ..B2buaConfig::default() }
}

/// Our Via on the dialled INVITE of `b-1`, stating `offer`, signed when
/// `signed`.
fn our_via(offer: bool, signed: bool) -> String {
    let k = key();
    let facts = RequestFacts { method: "INVITE", uri: "sip:bob@10.0.0.2:5070", offer };
    let via = build_call_via(
        &StackIdentityOpts {
            local_ip: "10.0.0.9",
            local_port: 5060,
            call_ref: "w0|c1@alice|atag",
            leg: "b-1",
            incarnation_mark: "k3x9",
            is_emergency: false,
            other_owner: None,
            request: signed.then_some((facts, &k)),
        },
        "z9hG4bKinvite".to_string(),
    );
    via.to_wire()
}

fn response(status: u16, via: &str, sdp: &str) -> SipResponse {
    let typed = if sdp.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    let raw = format!(
        "SIP/2.0 {status} Whatever\r\n\
Via: {via}\r\n\
From: <sip:svc@10.0.0.9>;tag=svc\r\n\
To: <sip:bob@10.0.0.2>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
{typed}Content-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("a response") {
        SipMessage::Response(r) => r,
        SipMessage::Request(_) => panic!("a response"),
    }
}

fn answer(resp: &SipResponse) -> Vec<OutboundSipEffect> {
    let src = ("10.0.0.5".to_string(), 5080);
    answer_unheld_response(&config(), resp, src, &mut TurnIds::fixed(1), 0)
}

fn requests(effects: &[OutboundSipEffect]) -> Vec<sip_message::SipRequest> {
    effects
        .iter()
        .filter_map(|e| match &e.body {
            OutboundBody::Request(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_2xx_is_acked_and_its_dialog_byed() {
    let sent = requests(&answer(&response(200, &our_via(true, true), "")));
    let methods: Vec<&str> = sent.iter().map(|r| r.method().as_str()).collect();
    assert_eq!(methods, ["ACK", "BYE"]);
    assert_eq!(sent[0].cseq().seq(), 1);
    assert_eq!(sent[1].cseq().seq(), 2);
    assert!(sent[0].body().is_empty(), "the INVITE carried the offer: a bare ACK");
}

#[test]
fn a_2xx_without_a_verified_extension_draws_nothing() {
    assert!(answer(&response(200, &our_via(true, false), "")).is_empty());
    let unkeyed = B2buaConfig::default();
    let resp = response(200, &our_via(true, true), "");
    let src = ("10.0.0.5".to_string(), 5080);
    let sent = answer_unheld_response(&unkeyed, &resp, src, &mut TurnIds::fixed(1), 0);
    assert!(sent.is_empty(), "a node without the cluster key trusts no extension");
}

#[test]
fn an_offer_in_a_2xx_to_an_invite_that_carried_none_is_answered_rejecting_every_stream() {
    let sent = requests(&answer(&response(200, &our_via(false, true), OFFER)));
    let ack = String::from_utf8_lossy(sent[0].body()).to_string();
    assert!(ack.contains("m=audio 0 "), "every stream rejected: {ack}");
}

#[test]
fn a_non_2xx_is_acked_on_its_own_branch_at_the_stated_uri() {
    let sent = requests(&answer(&response(487, &our_via(true, true), "")));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method().as_str(), "ACK");
    assert_eq!(sent[0].top_via().branch(), Some("z9hG4bKinvite"));
    assert_eq!(sent[0].request_uri().to_string(), "sip:bob@10.0.0.2:5070");
}

#[test]
fn a_non_2xx_without_a_verified_extension_draws_nothing() {
    assert!(answer(&response(487, &our_via(true, false), "")).is_empty());
}

/// The ACK and the BYE of the 2xx are this node's requests: their Via names
/// this node's sent-by and the call and leg the 2xx's Via names, never the
/// sent-by of the node that sent the INVITE nor what the next hop stamped on
/// it (RFC 3261 §8.1.1.7, §18.2.1, RFC 3581 §4).
#[test]
fn the_ack_and_the_bye_of_a_2xx_carry_this_nodes_via() {
    let stamped = our_via(true, true).replacen(";rport", ";rport=5061;received=10.0.0.8", 1);
    let config = B2buaConfig { sip_local_ip: "10.0.0.7".into(), sip_local_port: 5062, ..config() };
    let src = ("10.0.0.5".to_string(), 5080);
    let resp = response(200, &stamped, "");
    let sent = requests(&answer_unheld_response(&config, &resp, src, &mut TurnIds::fixed(1), 0));
    assert_eq!(sent.len(), 2, "the ACK and the BYE");
    let invite_hop = resp.top_via();
    for req in &sent {
        let hop = req.top_via();
        let method = req.method().as_str();
        assert_eq!(hop.host_port(), ("10.0.0.7", 5062), "{method}: this node's sent-by");
        assert_eq!(hop.received(), None, "{method}: nothing the next hop stamped");
        assert_eq!(hop.rport(), Some(Rport::Requested), "{method}: rport requested, not valued");
        assert_ne!(hop.branch(), invite_hop.branch(), "{method}: a transaction of its own");
        for mark in ["cr", "lg", "ci"] {
            assert_eq!(hop.param(mark), invite_hop.param(mark), "{method}: the call's {mark}");
        }
    }
}

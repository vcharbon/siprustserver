//! A non-2xx final to a leg's dialling INVITE another owner sent, on a call
//! this node holds but a leg it lacks (ADR-0055 §8): the dialog gate drops it
//! as a stray, its transaction layer sent no ACK, and the router ACKs it hop
//! by hop from the verified via-extension (RFC 3261 §17.1.1.3, ADR-0055 §12).
//! A final the layer matched, one whose Via names this node, a 2xx, a
//! provisional and one with no verified extension draw nothing here.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_sdk::event::CallEvent;
use sip_message::header::HeaderValue;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use super::test_support::{invite, node_on, src, Node, NODE_SIP_ADDR};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::stack_identity::{build_call_via, RequestFacts, StackIdentityOpts};
use crate::store::CallTopology;

fn key() -> sip_ids::SigningKey {
    sip_ids::SigningKey::new(&[5u8; 32]).expect("a 32-byte key")
}

/// The proxy every response reaches the node through.
const PROXY: ([u8; 4], u16) = ([127, 0, 0, 5], 5080);
/// The other owner, which dialled the leg.
const OTHER: (&str, u16) = ("127.0.0.3", 5092);
/// The callee the other owner dialled.
const CALLEE_URI: &str = "sip:carol@192.0.2.3:5070";
const BRANCH: &str = "z9hG4bK-other-dial";

/// A node serving a call `w1` backs up, resident without the leg `c-1`, and
/// the proxy's endpoint.
async fn holding_a_call() -> (Node, Box<dyn UdpEndpoint>, String, String) {
    let net = SimulatedSignalingNetwork::new(1);
    let node = node_on(&net, "w0", |c| c.cluster_key = Some(key()), Vec::new()).await;
    let proxy = net.bind_udp(BindUdpOpts::new(SocketAddr::from(PROXY), 64)).await.unwrap();
    let call = build_initial_call(
        &invite("w0", "w1", "unheld-final"),
        src(),
        &B2buaConfig { self_ordinal: "w0".into(), ..Default::default() },
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let ctx = node.core.router_ctx();
    ctx.state.create(call.clone(), Some(CallTopology::new("w0", "w1", 3, 1)));
    let mark = call.incarnation_mark().to_string();
    (node, proxy, call.call_ref, mark)
}

/// The callee's `status` to the INVITE sent on `c-1` from `sender`, its
/// via-extension signed under the cluster key when `signed`.
fn final_to_the_dial(
    call_ref: &str,
    mark: &str,
    sender: (&str, u16),
    status: u16,
    signed: bool,
) -> SipMessage {
    let k = key();
    let facts = RequestFacts { method: "INVITE", uri: CALLEE_URI, offer: true };
    let via = build_call_via(
        &StackIdentityOpts {
            local_ip: sender.0,
            local_port: sender.1,
            call_ref,
            leg: "c-1",
            incarnation_mark: mark,
            is_emergency: false,
            backup: Some("w1"),
            primary: Some("w0"),
            request: signed.then_some((facts, &k)),
        },
        BRANCH.to_string(),
    );
    let raw = format!(
        "SIP/2.0 {status} Whatever\r\nVia: {}\r\nFrom: <sip:svc@127.0.0.3>;tag=w1-tag\r\n\
         To: <sip:carol@192.0.2.3>;tag=caroltag\r\nCall-ID: c-1-call-id\r\nCSeq: 1 INVITE\r\n\
         Content-Length: 0\r\n\r\n",
        via.to_wire()
    );
    CustomParser::new().parse(raw.as_bytes()).expect("a response")
}

/// The requests `ep` receives within a second.
async fn heard(ep: &dyn UdpEndpoint) -> Vec<sip_message::SipRequest> {
    let mut requests = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while let Ok(Some(p)) = tokio::time::timeout_at(deadline, ep.recv()).await {
        if let Ok(SipMessage::Request(r)) = CustomParser::new().parse(&p.raw) {
            requests.push(r);
        }
    }
    requests
}

#[tokio::test(start_paused = true)]
async fn a_non_2xx_on_a_leg_the_held_call_lacks_is_acked_from_its_extension() {
    let (_node, proxy, call_ref, mark) = holding_a_call().await;
    let refused = final_to_the_dial(&call_ref, &mark, OTHER, 487, true);
    let SipMessage::Response(resp) = &refused else { unreachable!() };
    proxy.send_to(resp.image(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    let acks = heard(proxy.as_ref()).await;
    assert_eq!(acks.len(), 1, "one ACK: {acks:?}");
    assert_eq!(acks[0].method().as_str(), "ACK");
    assert_eq!(acks[0].request_uri().text(), CALLEE_URI, "at the Request-URI the extension states");
    assert_eq!(acks[0].top_via().branch(), Some(BRANCH), "on the INVITE's branch");
}

/// What the router's step sends for each final that is owed nothing here.
#[tokio::test(start_paused = true)]
async fn finals_owed_nothing_here_draw_no_ack() {
    let (node, proxy, call_ref, mark) = holding_a_call().await;
    let ctx = node.core.router_ctx();
    let this = ("127.0.0.2", NODE_SIP_ADDR.1);
    let cases = [
        ("a Via naming this node", this, 487, true, false),
        ("a final the layer matched", OTHER, 487, true, true),
        ("a 2xx", OTHER, 200, true, false),
        ("a provisional", OTHER, 180, true, false),
        ("no verified extension", OTHER, 487, false, false),
    ];
    for (what, sender, status, signed, matched) in cases {
        let message = final_to_the_dial(&call_ref, &mark, sender, status, signed);
        let event = CallEvent::sip(message, SocketAddr::from(PROXY));
        let mut ids = sip_ids::TurnIds::fixed(1);
        super::unheld::answer_unheld_final(ctx, &event, matched, &mut ids).await;
        assert!(heard(proxy.as_ref()).await.is_empty(), "{what}: nothing sent");
    }
}

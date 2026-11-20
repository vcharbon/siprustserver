//! A final to a leg's dialling INVITE another owner sent, on a call this node
//! holds, that names no dialog this node holds of the leg — a leg it lacks,
//! or one whose own dial it holds under other tags (ADR-0055 §8): the dialog
//! gate drops it as a stray, its transaction layer sent no ACK, and the
//! router answers it from the verified via-extension (ADR-0055 §12): a
//! non-2xx is ACKed hop by hop (RFC 3261 §17.1.1.3), a 2xx ACKed and its
//! dialog BYEd (§13.2.2.4, §15.1.1). A final the layer matched, one whose Via
//! names this node, a provisional and one with no verified extension draw
//! nothing here.

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

/// The callee's endpoint, where an unheld 2xx's ACK and BYE go: its Contact.
const CALLEE: ([u8; 4], u16) = ([192, 0, 2, 3], 5070);

/// A node serving a call `w1` backs up, resident with the leg `b-1` it dialled
/// itself, confirmed under its own tags, and without the leg `c-1`; the
/// proxy's endpoint and the callee's.
async fn holding_a_call() -> (Node, Box<dyn UdpEndpoint>, Box<dyn UdpEndpoint>, String, String) {
    holding(|_| {}).await
}

/// The same, the record also holding the leg `c-1` `w1` dialled, ended by
/// the CANCEL its INVITE drew: a final to that dial passes the gate to the
/// call's turn.
async fn holding_the_other_owners_dial(
) -> (Node, Box<dyn UdpEndpoint>, Box<dyn UdpEndpoint>, String, String) {
    holding(|call| {
        let mut leg = call::testkit::dialled_leg("c-1", "w1-tag");
        if let Some(record) = leg.invite.as_mut() {
            record.relayed_by = call::RelayOwner::new("w1", "w1-boot-1");
        }
        call::testkit::stage_leg(&mut leg, call::LegState::Terminated);
        call::helpers::add_b_leg(call, leg);
    })
    .await
}

/// A node holding the call [`holding_a_call`] states, `more` added to its
/// record before the node takes it.
async fn holding(
    more: impl FnOnce(&mut call::Call),
) -> (Node, Box<dyn UdpEndpoint>, Box<dyn UdpEndpoint>, String, String) {
    let net = SimulatedSignalingNetwork::new(1);
    let node = node_on(&net, "w0", |c| c.cluster_key = Some(key()), Vec::new()).await;
    let proxy = net.bind_udp(BindUdpOpts::new(SocketAddr::from(PROXY), 64)).await.unwrap();
    let callee = net.bind_udp(BindUdpOpts::new(SocketAddr::from(CALLEE), 64)).await.unwrap();
    let mut call = build_initial_call(
        &invite("w0", "w1", "unheld-final"),
        src(),
        &B2buaConfig { self_ordinal: "w0".into(), ..Default::default() },
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut own = call::testkit::dialled_leg("b-1", "w0-tag");
    own.dialogs.insert(call::Dialog::new("w0-tag", "carol-w0", call::DialogState::Confirmed, 1));
    call::helpers::add_b_leg(&mut call, own);
    more(&mut call);
    let ctx = node.core.router_ctx();
    ctx.state.create(call.clone(), Some(CallTopology::new("w0", "w1", 3, 1)));
    let mark = call.incarnation_mark().to_string();
    (node, proxy, callee, call.call_ref, mark)
}

/// The callee's `status` to the INVITE `w1` sent on `leg` from `sender`, under
/// `w1`'s tags, its via-extension signed under the cluster key when `signed`.
fn final_to_the_dial(
    call_ref: &str,
    mark: &str,
    sender: (&str, u16),
    status: u16,
    signed: bool,
) -> SipMessage {
    final_on(call_ref, mark, "c-1", sender, status, signed)
}

fn final_on(
    call_ref: &str,
    mark: &str,
    leg: &str,
    sender: (&str, u16),
    status: u16,
    signed: bool,
) -> SipMessage {
    let k = key();
    let facts = RequestFacts { method: "INVITE", uri: CALLEE_URI, route: "", offer: true };
    let via = build_call_via(
        &StackIdentityOpts {
            local_ip: sender.0,
            local_port: sender.1,
            call_ref,
            leg,
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
         Contact: <sip:carol@192.0.2.3:5070>\r\nContent-Length: 0\r\n\r\n",
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
    let (_node, proxy, callee, call_ref, mark) = holding_a_call().await;
    let refused = final_to_the_dial(&call_ref, &mark, OTHER, 487, true);
    let SipMessage::Response(resp) = &refused else { unreachable!() };
    proxy.send_to(resp.image(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    // With no front proxy the ACK goes where the INVITE went (RFC 3261 §17.1.1.2).
    let acks = heard(callee.as_ref()).await;
    assert_eq!(acks.len(), 1, "one ACK: {acks:?}");
    assert_eq!(acks[0].method().as_str(), "ACK");
    assert_eq!(acks[0].request_uri().text(), CALLEE_URI, "at the Request-URI the extension states");
    assert_eq!(acks[0].top_via().branch(), Some(BRANCH), "on the INVITE's branch");
    assert!(proxy.try_recv().is_none(), "nothing reaches the socket the final came from");
}

/// What the router's step sends for each final that is owed nothing here.
#[tokio::test(start_paused = true)]
async fn finals_owed_nothing_here_draw_no_ack() {
    let (node, proxy, callee, call_ref, mark) = holding_a_call().await;
    let ctx = node.core.router_ctx();
    let this = ("127.0.0.2", NODE_SIP_ADDR.1);
    let cases = [
        ("a Via naming this node", this, 487, true, false),
        ("a final the layer matched", OTHER, 487, true, true),
        ("a provisional", OTHER, 180, true, false),
        ("no verified extension", OTHER, 487, false, false),
    ];
    for (what, sender, status, signed, matched) in cases {
        let message = final_to_the_dial(&call_ref, &mark, sender, status, signed);
        let event = CallEvent::sip(message, SocketAddr::from(PROXY));
        let mut ids = sip_ids::TurnIds::fixed(1);
        super::unheld::answer_unheld_final(ctx, &event, matched, &mut ids).await;
        assert!(heard(proxy.as_ref()).await.is_empty(), "{what}: nothing sent");
        assert!(heard(callee.as_ref()).await.is_empty(), "{what}: nothing to the callee");
    }
}

/// A 2xx to the INVITE `w1` sent on `b-1`, a leg this node dialled itself and
/// holds a dialog of under its own tags: the 2xx's tags name no dialog of
/// it, so it is ACKed and its dialog BYEd from the via-extension.
#[tokio::test(start_paused = true)]
async fn a_2xx_naming_no_dialog_of_the_held_leg_is_acked_and_byed_from_its_extension() {
    let (_node, proxy, callee, call_ref, mark) = holding_a_call().await;
    let answered = final_on(&call_ref, &mark, "b-1", OTHER, 200, true);
    let SipMessage::Response(resp) = &answered else { unreachable!() };
    proxy.send_to(resp.image(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    // The callee leaves the BYE unanswered, so its client transaction
    // retransmits it on its branch (RFC 3261 §17.1.2.2): one request each.
    let mut sent: Vec<(String, String)> = heard(callee.as_ref())
        .await
        .iter()
        .map(|r| (r.method().as_str().to_string(), r.top_via().branch().unwrap_or_default().into()))
        .collect();
    sent.dedup();
    let methods: Vec<&str> = sent.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(methods, ["ACK", "BYE"], "the 2xx is ACKed and its dialog BYEd: {sent:?}");
}

/// A 2xx naming the dialog this node holds of `b-1` passes the gate to the
/// call's turn: no BYE ends the dialog the call holds.
#[tokio::test(start_paused = true)]
async fn a_2xx_naming_the_held_dialog_keeps_its_own_path() {
    let (_node, proxy, callee, call_ref, mark) = holding_a_call().await;
    let answered = final_on(&call_ref, &mark, "b-1", OTHER, 200, true);
    let SipMessage::Response(resp) = &answered else { unreachable!() };
    let own = String::from_utf8_lossy(resp.image())
        .replace("tag=w1-tag", "tag=w0-tag")
        .replace("tag=caroltag", "tag=carol-w0");
    proxy.send_to(own.as_bytes(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
    let byes = heard(callee.as_ref()).await.into_iter().filter(|r| r.method().as_str() == "BYE");
    assert_eq!(byes.count(), 0, "the held dialog is not BYEd");
}

/// The ACKs and the BYEs, each BYE once whatever its retransmissions, that
/// `ep` receives within a second.
async fn acks_and_byes(ep: &dyn UdpEndpoint) -> (usize, usize) {
    let heard = heard(ep).await;
    let acks = heard.iter().filter(|r| r.method().as_str() == "ACK").count();
    let mut byes: Vec<&str> = heard
        .iter()
        .filter(|r| r.method().as_str() == "BYE")
        .filter_map(|r| r.top_via().branch())
        .collect();
    byes.sort_unstable();
    byes.dedup();
    (acks, byes.len())
}

/// Sends `copies` copies of `resp` from `proxy` to the node, 1 ms apart: a
/// peer re-sending its final far faster than its ladder.
async fn burst(proxy: &dyn UdpEndpoint, resp: &SipMessage, copies: usize) {
    let SipMessage::Response(resp) = resp else { unreachable!() };
    for _ in 0..copies {
        proxy.send_to(resp.image(), SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// The copies of one final a compliant answerer sends after the first (RFC
/// 3261 §13.3.1.4, §17.2.1; T1 = 500 ms, T2 = 4 s).
const COMPLIANT_COPIES: usize = 10;

/// A held call's final naming no leg or dialog it holds, re-sent at wire
/// speed: the first arrival and the copies a compliant answerer sends draw an
/// ACK each, the rest inside T1 nothing (ADR-0063 §2), counted as absorbed.
#[tokio::test(start_paused = true)]
async fn a_held_calls_unheld_final_repeated_at_wire_speed_draws_bounded_acks() {
    let (node, proxy, callee, call_ref, mark) = holding_a_call().await;
    burst(proxy.as_ref(), &final_to_the_dial(&call_ref, &mark, OTHER, 487, true), 15).await;
    let (acks, byes) = acks_and_byes(callee.as_ref()).await;
    assert_eq!((acks, byes), (COMPLIANT_COPIES + 1, 0), "a non-2xx: ACKs on its ladder, no BYE");
    let metrics = &node.core.router_ctx().metrics;
    assert_eq!(metrics.copies_past_schedule_total("unheld"), 4, "the copies past it are counted");
}

/// The same for a 2xx naming no dialog of the held leg: its dialog is BYEd
/// once, on the first arrival.
#[tokio::test(start_paused = true)]
async fn a_held_calls_unheld_2xx_repeated_at_wire_speed_is_byed_once() {
    let (node, proxy, callee, call_ref, mark) = holding_a_call().await;
    burst(proxy.as_ref(), &final_on(&call_ref, &mark, "b-1", OTHER, 200, true), 15).await;
    let (acks, byes) = acks_and_byes(callee.as_ref()).await;
    assert_eq!((acks, byes), (COMPLIANT_COPIES + 1, 1), "a 2xx: ACKs on its ladder, one BYE");
    let metrics = &node.core.router_ctx().metrics;
    assert_eq!(metrics.copies_past_schedule_total("unheld"), 4, "the copies past it are counted");
}

/// A non-2xx to the dial another owner sent on a leg the held call holds,
/// re-sent at wire speed: each copy passes the gate to the call's turn, whose
/// ACK the book bounds as the router's own (ADR-0063 §2); a turn whose ACK
/// the book absorbed leaves the record as it read it and persists quietly.
#[tokio::test(start_paused = true)]
async fn a_held_calls_turn_on_a_final_repeated_at_wire_speed_draws_bounded_acks() {
    let (node, proxy, callee, call_ref, mark) = holding_the_other_owners_dial().await;
    burst(proxy.as_ref(), &final_to_the_dial(&call_ref, &mark, OTHER, 487, true), 15).await;
    let (acks, byes) = acks_and_byes(callee.as_ref()).await;
    assert_eq!((acks, byes), (COMPLIANT_COPIES + 1, 0), "a non-2xx: ACKs on its ladder, no BYE");
    let metrics = &node.core.router_ctx().metrics;
    assert_eq!(metrics.copies_past_schedule_total("unheld"), 4, "the copies past it are counted");
    assert_eq!(metrics.repl_quiet_turns_total("absorbed-copy"), 4, "their turns are quiet");
}

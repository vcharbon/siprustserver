//! A resident call whose record stops rebuilding its caller's INVITE
//! (ADR-0052 §11): the turn that reads it discharges the call silently, and
//! the request that met it is answered as one for a vanished call.

use std::net::SocketAddr;
use std::time::Duration;

use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, node_on, src, NODE_SIP_ADDR};

/// The callee the scripted decision routes every call to.
const CALLEE: ([u8; 4], u16) = ([127, 0, 0, 2], 9);

/// The next datagram `ep` receives within a second, as text.
async fn next(ep: &dyn UdpEndpoint) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let packet = tokio::time::timeout_at(deadline, ep.recv()).await;
    let packet = packet.expect("a datagram in time").expect("the endpoint is open");
    String::from_utf8_lossy(&packet.raw).to_string()
}

/// The first datagram `ep` receives within a second whose first line starts
/// with `start`.
async fn next_starting(ep: &dyn UdpEndpoint, start: &str) -> String {
    loop {
        let text = next(ep).await;
        if text.starts_with(start) {
            return text;
        }
    }
}

/// The value of `text`'s first header line `name`.
fn header<'a>(text: &'a str, name: &str) -> &'a str {
    let prefix = format!("{name}:");
    let line = text.lines().find(|l| l.starts_with(&prefix)).expect("the header is present");
    line[prefix.len()..].trim()
}

/// The 180 `to_tag` answers `invite` with.
fn ringing(invite: &str, to_tag: &str) -> String {
    let vias: String =
        invite.lines().filter(|l| l.starts_with("Via:")).map(|l| format!("{l}\r\n")).collect();
    format!(
        "SIP/2.0 180 Ringing\r\n{vias}From: {}\r\nTo: {};tag={to_tag}\r\nCall-ID: {}\r\nCSeq: {}\r\nContact: <sip:bob@127.0.0.2:9>\r\nContent-Length: 0\r\n\r\n",
        header(invite, "From"),
        header(invite, "To"),
        header(invite, "Call-ID"),
        header(invite, "CSeq"),
    )
}

/// **The trigger is answered.** The caller hangs up in the early dialog of a
/// call whose record no longer rebuilds her INVITE: the turn that would answer
/// that INVITE meets the defect, the call is discharged with nothing else on
/// the wire, counted, and her BYE draws the 481 of a request naming no call.
#[tokio::test(start_paused = true)]
async fn a_bye_to_a_call_whose_record_does_not_rebuild_its_invite_draws_481() {
    let net = SimulatedSignalingNetwork::new(1);
    let mut n = node_on(&net, "w0", |_| {}, Vec::new()).await;
    let caller = net.bind_udp(BindUdpOpts::new(src(), 64)).await.unwrap();
    let callee = net.bind_udp(BindUdpOpts::new(SocketAddr::from(CALLEE), 64)).await.unwrap();
    let node = SocketAddr::from(NODE_SIP_ADDR);
    let ctx = n.core.router_ctx();

    let offer = invite("w0", "w1", "unrecoverable");
    caller.send_to(offer.image(), node).await.unwrap();
    let dialled = next_starting(callee.as_ref(), "INVITE ").await;
    callee.send_to(ringing(&dialled, "bobtag").as_bytes(), node).await.unwrap();
    let shown = next_starting(caller.as_ref(), "SIP/2.0 180").await;
    let a_face = header(&shown, "To").split(";tag=").nth(1).expect("the 180 shows a tag");
    let contact = header(&shown, "Contact").trim_start_matches('<').trim_end_matches('>');

    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let born = build_initial_call(&offer, src(), &config, &mut sip_ids::TurnIds::fixed(1), 0);
    let call_ref = born.call_ref;
    let mut call = ctx.state.peek(&call_ref).expect("the call is resident");
    call.a_leg_invite.headers.retain(|h| !h.name.eq_ignore_ascii_case("CSeq"));
    ctx.state.update_quiet(call);

    let bye = format!(
        "BYE {contact} SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-bye\r\nMax-Forwards: 70\r\nFrom: <sip:alice@example.com>;tag=alicetag\r\nTo: <sip:bob@example.com>;tag={a_face}\r\nCall-ID: unrecoverable@10.0.0.9\r\nCSeq: 2 BYE\r\nContent-Length: 0\r\n\r\n"
    );
    caller.send_to(bye.as_bytes(), node).await.unwrap();
    let answer = next_starting(caller.as_ref(), "SIP/2.0 ").await;
    sip_clock::testkit::settle().await;

    assert!(answer.starts_with("SIP/2.0 481"), "the BYE draws 481, got {answer:?}");
    assert_eq!(header(&answer, "CSeq"), "2 BYE");
    assert!(ctx.state.peek(&call_ref).is_none(), "the call is discharged");
    assert_eq!(n.metrics.unrecoverable_calls_total(), 1);
    assert_eq!(n.cdr.snapshot().len(), 1, "the discharge writes the call's CDR");
    let defects = n.take_violations();
    assert_eq!(defects.len(), 1, "{defects:#?}");
    assert!(defects[0].starts_with("model defect caller-invite-rebuilds"), "{defects:#?}");
}

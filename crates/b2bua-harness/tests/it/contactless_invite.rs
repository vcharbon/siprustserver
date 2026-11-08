//! A new INVITE that states no Contact (RFC 3261 §8.1.1.8). Its dialog would
//! have no remote target, so the B2BUA refuses it 400 where it would establish
//! the dialog: after the decision, before any b-leg. A refusal that comes
//! first wins: a spent hop count is answered 483 (§16.3), however the INVITE
//! lacks a Contact. The caller here breaks §8.1.1.8 on purpose; the B2BUA's
//! own messages stay compliant.

use std::time::Duration;

use b2bua_harness::B2buaScene;
use scenario_harness::Agent;
use sip_message::{SipMessage, SipResponse};

/// The caller's deliberate defect, waived on its side alone.
const WAIVER: &str =
    "the caller's INVITE states no Contact on purpose: the test is the B2BUA's answer to it";

/// A new INVITE from `from` to `dst` with no Contact and no body.
fn contactless_invite(from: &Agent, dst: std::net::SocketAddr, hops: u32, tag: &str) -> String {
    format!(
        "INVITE sip:bob@{dst} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via};branch=z9hG4bK-{tag}\r\n\
         Max-Forwards: {hops}\r\n\
         From: <sip:alice@{via}>;tag=a-{tag}\r\n\
         To: <sip:bob@{dst}>\r\n\
         Call-ID: {tag}@{via}\r\n\
         CSeq: 1 INVITE\r\n\
         Content-Length: 0\r\n\r\n",
        via = from.addr(),
    )
}

/// The ACK of a non-2xx final `resp` to `invite` (RFC 3261 §17.1.1.3): the
/// INVITE's Request-URI, Via, From, Call-ID and CSeq number, the final's To.
fn ack_for(invite: &str, resp: &SipResponse) -> String {
    let line = |name: &str| {
        invite.lines().find(|l| l.starts_with(name)).expect("the INVITE line").to_string()
    };
    let ruri = invite.lines().next().expect("a start line").split(' ').nth(1).expect("a URI");
    format!(
        "ACK {ruri} SIP/2.0\r\n{}\r\nMax-Forwards: 70\r\n{}\r\nTo: {}\r\n{}\r\nCSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n",
        line("Via:"),
        line("From:"),
        sip_message::header::HeaderValue::to_wire(resp.to()),
        line("Call-ID:"),
    )
}

/// The one final `agent` received for its INVITE, once the fabric delivered it.
async fn final_for_invite(s: &B2buaScene, agent: &Agent) -> SipResponse {
    s.h.advance(Duration::from_millis(500)).await;
    let mut finals = Vec::new();
    while let Some(msg) = agent.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.status() >= 200 {
                finals.push(r);
            }
        }
    }
    finals.into_iter().next().expect("the INVITE draws a final")
}

async fn answered(s: &B2buaScene, hops: u32, tag: &str) -> u16 {
    s.h.allow_violation("contact-presence", WAIVER);
    let invite = contactless_invite(&s.alice, s.b2bua.addr, hops, tag);
    s.alice.try_send_datagram(invite.as_bytes(), s.b2bua.addr).await.expect("sent");
    let resp = final_for_invite(s, &s.alice).await;
    let ack = ack_for(&invite, &resp);
    s.alice.try_send_datagram(ack.as_bytes(), s.b2bua.addr).await.expect("sent");
    s.bob.drain_expecting(Duration::from_millis(50), &[]).await;
    resp.status()
}

/// A routable INVITE with no Contact is answered 400 and no b-leg is dialled.
#[tokio::test(start_paused = true)]
async fn a_contactless_invite_is_answered_400_before_any_b_leg() {
    let s = B2buaScene::new("b2bua-contactless-invite-400").await;
    assert_eq!(answered(&s, 70, "contactless").await, 400);
    s.finish().await;
}

/// A spent hop count comes first: the route is refused 483, as for any INVITE
/// at `Max-Forwards: 0`, whether or not it states a Contact.
#[tokio::test(start_paused = true)]
async fn a_contactless_invite_at_max_forwards_zero_is_answered_483() {
    let s = B2buaScene::new("b2bua-contactless-invite-483").await;
    assert_eq!(answered(&s, 0, "contactless-probe").await, 483);
    s.finish().await;
}

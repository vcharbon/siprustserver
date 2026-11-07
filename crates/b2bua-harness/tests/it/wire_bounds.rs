//! ADR-0001's bounds on a full call through the front proxy and a worker: a
//! header long enough for what real traffic carries, a To at its old bound, and
//! a request at the line bound, whose forward the worker still reads.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness};
use sip_message::SipParserLimits;

use crate::common;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const PROXY: &str = "127.0.0.1:5080";
const B2BUA: &str = "127.0.0.1:5090";

struct Rig {
    h: Harness,
    alice: Agent,
    bob: Agent,
    proxy: common::ProxySut,
    b2bua: B2buaSut,
}

impl Rig {
    async fn new(name: &str) -> Rig {
        let h = Harness::new(name);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let proxy = common::spawn_lb_proxy(&h, PROXY, "b2bua", B2BUA.parse().unwrap()).await;
        let b2bua = B2buaSut::route_all_to("127.0.0.1", 5070)
            .behind_proxy("127.0.0.1", 5080)
            .start(&h, "b2bua", B2BUA)
            .await;
        Rig { h, alice, bob, proxy, b2bua }
    }

    async fn finish(self) -> scenario_harness::RunReport {
        settle_until(|| self.b2bua.cdr_records().len() == 1).await;
        settle_until(|| self.b2bua.is_reaped()).await;
        self.b2bua.assert_fully_reaped();
        let _proxy = self.proxy;
        self.h.finish().await
    }
}

/// `body`, a value of exactly `len` bytes.
fn pad(len: usize) -> String {
    "a".repeat(len)
}

/// A caller's `Identity` of 2,600 bytes, which real traffic carries, does not
/// keep the call from being set up and torn down.
#[tokio::test(start_paused = true)]
async fn a_2600_byte_identity_header_on_a_full_call() {
    let r = Rig::new("wire-bounds-identity").await;
    let identity = format!("{};info=<https://cert.example.com/a.pem>;alg=ES256", pad(2_560));
    assert!(identity.len() > 2_590 && identity.len() < 2_620, "{}", identity.len());

    let mut call = r
        .alice
        .invite(&r.bob)
        .with_header("Identity", &identity)
        .with_sdp(OFFER)
        .through(r.proxy.addr())
        .send()
        .await;
    r.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    r.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    r.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    r.finish().await;
}

/// A To of 2,048 bytes draws its final, whose To grows by the callee's tag and
/// the stack's: the caller is answered, and the worker's stored 486 reads back.
#[tokio::test(start_paused = true)]
async fn a_2048_byte_to_draws_its_final() {
    let r = Rig::new("wire-bounds-to").await;
    let frame = "To: <sip:@example.com>".len();
    let to = format!("sip:{}@example.com", pad(2_048 - frame));

    let mut call =
        r.alice.invite(&r.bob).to(&to).with_sdp(OFFER).through(r.proxy.addr()).send().await;
    r.bob.receive("INVITE").await.respond(486, "Busy Here").await;
    call.expect(486).await;
    r.finish().await;
}

/// An INVITE at exactly the line bound crosses the proxy, which adds its Via
/// and its Record-Route, and the worker reads what the proxy forwards: the call
/// is set up and torn down. The INVITE carries a lower hop's Via, which the
/// worker's b-leg leaves out, so that b-leg, Route included, stands at the bound
/// when it reaches the proxy in turn; the callee reads it past the bound, with
/// the proxy's additions.
#[tokio::test(start_paused = true)]
async fn a_request_at_the_line_bound_crosses_proxy_and_worker() {
    let r = Rig::new("wire-bounds-lines").await;
    let limit = SipParserLimits::default().max_header_lines;
    r.bob.reads_under(SipParserLimits::default().behind_proxy());
    // The UA's own lines: the request line and nine headers.
    const OWN_LINES: usize = 10;
    let lower_hop = "SIP/2.0/UDP 192.0.2.40:5060;branch=z9hG4bK-lower-hop";
    let (alice, bob) = (&r.alice, &r.bob);
    let mut invite =
        alice.invite(bob).with_sdp(OFFER).through(r.proxy.addr()).with_header("Via", lower_hop);
    for n in 0..(limit - OWN_LINES - 1) {
        invite = invite.with_header(&format!("X-Pad-{n}"), "p");
    }
    let mut call = invite.send().await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let proxy_addr: std::net::SocketAddr = PROXY.parse().unwrap();
    let worker_addr: std::net::SocketAddr = B2BUA.parse().unwrap();
    let report = r.finish().await;
    let entries = report.entries();
    let lines_on = |at: std::net::SocketAddr| {
        let invite = entries.iter().find(|e| e.to == at && e.raw.starts_with(b"INVITE ")).unwrap();
        let text = String::from_utf8_lossy(&invite.raw).to_string();
        text.split("\r\n\r\n").next().unwrap_or_default().lines().count()
    };
    assert_eq!(lines_on(proxy_addr), limit, "the caller's INVITE stands at the bound");
    assert!(lines_on(worker_addr) > limit, "the forward carries the proxy's additions");
}

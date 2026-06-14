//! End-to-end: `alice → proxy → b2bua → proxy → bob`, with a single real
//! load-balancing `ProxyCore` in front of one real `B2buaCore` worker, both
//! bound as SUTs on the scenario-harness simulated network. Port of the
//! sipjsserver `basic-call` scenario run on the `proxy+b2b` SUT
//! (`tests/support/proxyB2bFakeStack.ts`, single-worker form): the B2BUA is
//! deployed behind the front proxy (`b2bOutboundProxy`), so the b-leg
//! (worker→bob) traffic *also* traverses the proxy — symmetric with the
//! a-leg's cookie-decoded path.
//!
//! Topology / addressing:
//!
//!   alice 127.0.0.1:5060 ──▶ proxy :5080 ──▶ b2bua :5090 ──▶ proxy :5080 ──▶ bob :5070
//!
//! - The proxy HRW-routes alice's new-dialog INVITE to the (only) worker and
//!   inserts a signed Record-Route cookie; alice learns that route set off the
//!   200 OK, so her in-dialog BYE returns through the proxy (cookie decode).
//! - The worker's b-leg INVITE/ACK/BYE carry a preloaded `;outbound` Route at
//!   the proxy (R-URI stays bob); the proxy classifies them worker-outbound and
//!   forwards to the R-URI.
//!
//! The test asserts both the call lifecycle (CDR with answer + bye) AND that
//! the INVITE and BYE each really made all four hops on the recording.

mod common;

use call::CdrEventType;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B2BUA: &str = "127.0.0.1:5090";

#[tokio::test]
async fn alice_calls_bob_through_proxy_and_b2bua() {
    let h = Harness::with_transit_delay("proxy-b2bua-basic", 0).describe(
        "alice → proxy → b2bua → proxy → bob: LB proxy fronts one B2BUA worker; \
         the worker's b-leg traffic traverses the proxy via b2bOutboundProxy.",
    );
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    // The proxy fronts the single B2BUA worker (HRW always picks it).
    let proxy = common::spawn_lb_proxy(&h, PROXY, "b2bua", B2BUA.parse().unwrap()).await;
    // The worker routes every call to bob, but sends its b-leg through the proxy.
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5070)
        .outbound_proxy("127.0.0.1", 5080)
        .start(&h, "b2bua", B2BUA)
        .await;

    // alice INVITEs bob, but sends through the proxy (it HRW-routes to the worker).
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;

    // The INVITE arrives at bob (proxy → b2bua → proxy → bob).
    let mut uas = bob.receive("INVITE").await;
    assert!(!uas.request().body.is_empty(), "offer relayed to bob");
    // The LB proxy Record-Routes the b-leg INVITE so it stays in the path for
    // the whole call — bob echoes this and any bob-initiated in-dialog request
    // decodes the cookie back to the worker.
    assert!(
        get_header(&uas.request().headers, "record-route")
            .is_some_and(|rr| rr.contains("127.0.0.1:5080") && rr.contains(";lr")),
        "LB proxy must Record-Route the b-leg INVITE, got {:?}",
        get_header(&uas.request().headers, "record-route"),
    );

    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "answer relayed to alice");
    // The a-leg 200 OK echoes the proxy's Record-Route, so alice's route set is
    // [<proxy;lr>] and her in-dialog BYE returns through the proxy.
    assert!(
        get_header(&ok.headers, "record-route")
            .is_some_and(|rr| rr.contains("127.0.0.1:5080") && rr.contains(";lr")),
        "a-leg 200 OK must echo the proxy Record-Route, got {:?}",
        get_header(&ok.headers, "record-route"),
    );

    // ACK end-to-end.
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // alice hangs up: BYE traverses proxy → b2bua, then the worker BYEs bob
    // through the proxy.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    // Let the worker drain teardown + CDR.
    settle_until(|| b2bua.cdr_records().len() == 1).await;

    // ── CDR assertions (call really established + tore down) ─────────────────
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR per call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::InviteReceived), "invite_received: {kinds:?}");
    assert!(kinds.contains(&CdrEventType::Answer), "answer: {kinds:?}");
    assert!(kinds.contains(&CdrEventType::Bye), "bye: {kinds:?}");
    assert_eq!(cdrs[0].b_legs.len(), 1, "one b-leg");

    // ── Routing assertions (every message made all four hops) ────────────────
    let report = h.finish().await;
    let entries = report.entries();
    assert!(entries.iter().all(|e| e.delivered), "all hops delivered");

    let alice_a: std::net::SocketAddr = ALICE.parse().unwrap();
    let bob_a: std::net::SocketAddr = BOB.parse().unwrap();
    let proxy_a: std::net::SocketAddr = PROXY.parse().unwrap();
    let b2bua_a: std::net::SocketAddr = B2BUA.parse().unwrap();

    // INVITE: alice → proxy → b2bua → proxy → bob.
    assert_hop(&entries, alice_a, proxy_a, "INVITE", "alice → proxy INVITE");
    assert_hop(&entries, proxy_a, b2bua_a, "INVITE", "proxy → b2bua INVITE (a-leg)");
    assert_hop(&entries, b2bua_a, proxy_a, "INVITE", "b2bua → proxy INVITE (b-leg)");
    assert_hop(&entries, proxy_a, bob_a, "INVITE", "proxy → bob INVITE");

    // BYE: alice → proxy → b2bua → proxy → bob (both legs through the proxy).
    assert_hop(&entries, alice_a, proxy_a, "BYE", "alice → proxy BYE");
    assert_hop(&entries, proxy_a, b2bua_a, "BYE", "proxy → b2bua BYE (a-leg)");
    assert_hop(&entries, b2bua_a, proxy_a, "BYE", "b2bua → proxy BYE (b-leg)");
    assert_hop(&entries, proxy_a, bob_a, "BYE", "proxy → bob BYE");

    // The b-leg INVITE bob ultimately receives keeps its Request-URI at bob
    // (loose routing: the proxy never rewrote the R-URI).
    let bob_invite = find_request(&entries, proxy_a, bob_a, "INVITE").expect("proxy → bob INVITE");
    assert!(
        bob_invite.uri.contains("@127.0.0.1:5070"),
        "INVITE R-URI at bob, got {}",
        bob_invite.uri
    );

    // ── Render the HTML / SVG / txt reports (clickable sequence diagram +
    // global + per-endpoint traces) under the crate's target tmp dir. ─────────
    let out = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("proxy-b2bua-basic");
    let _ = std::fs::remove_dir_all(&out);
    let written = scenario_harness::report::write_all(&report, &out).unwrap();
    eprintln!("\nproxy+b2bua report artifacts:");
    for p in &written {
        eprintln!("  {}", p.display());
    }
}

/// Assert at least one delivered request `method` flowed `from → to`.
fn assert_hop(
    entries: &[sip_net::RecordedSipEntry],
    from: std::net::SocketAddr,
    to: std::net::SocketAddr,
    method: &str,
    label: &str,
) {
    assert!(
        find_request(entries, from, to, method).is_some(),
        "missing hop: {label} ({from} → {to} {method})"
    );
}

/// Find the first request `method` on the `from → to` hop.
fn find_request(
    entries: &[sip_net::RecordedSipEntry],
    from: std::net::SocketAddr,
    to: std::net::SocketAddr,
    method: &str,
) -> Option<sip_message::SipRequest> {
    entries.iter().find_map(|e| {
        if e.from != from || e.to != to {
            return None;
        }
        match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Request(r)) if r.method == method => Some(r),
            _ => None,
        }
    })
}

//! Record-routing proxy / LB end-to-end — the routing behavior we need to test
//! the front proxy.
//!
//! Topology: `alice ──▶ proxy ──▶ bob`. The proxy adds a Via to forwarded
//! requests (so responses return through it), inserts a `;lr` Record-Route on
//! the dialog-creating INVITE, and strips its own Route/Via on the way through.
//! Alice learns the route set from the 200's Record-Route (UAC reversal), so
//! her ACK and BYE carry `Route: <proxy;lr>`, keep the Request-URI at bob's
//! contact, and are sent **to the proxy** (the top route), which strips its
//! Route and forwards to bob.
//!
//! This exercises exactly the strict/loose-routing port in
//! `sip-message::generators` + the route-set construction in the fluent harness.

use scenario_harness::Harness;
use sip_message::message_helpers::{get_header, get_headers};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

fn parse(raw: &[u8]) -> SipMessage {
    CustomParser::new().parse(raw).expect("entry parses")
}

#[tokio::test]
async fn record_routed_call_through_proxy() {
    let h = Harness::new("rr-proxy-call").describe(
        "alice → proxy → bob, proxy record-routes; ACK and BYE traverse the \
         proxy via the learned route set (loose routing, ;lr).",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let proxy = h.proxy("proxy", "127.0.0.1:5080").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let proxy_addr = proxy.addr();
    let alice_addr = alice.addr();
    let bob_addr = bob.addr();

    // INVITE: alice addresses bob but sends through the proxy.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy_addr).send().await;
    let fwd_invite = proxy.forward_request(bob_addr).await; // proxy RRs + Vias, → bob
    assert!(
        get_header(&fwd_invite.headers, "record-route")
            .is_some_and(|rr| rr.contains("127.0.0.1:5080") && rr.contains(";lr")),
        "proxy must insert a ;lr Record-Route"
    );

    let mut uas = bob.receive("INVITE").await;
    // bob's INVITE carries the proxy's Record-Route (which it echoes on responses).
    assert!(
        get_header(&uas.request().headers, "record-route").is_some(),
        "the INVITE bob received must carry the proxy's Record-Route"
    );

    uas.respond(180, "Ringing").await;
    proxy.forward_response(alice_addr).await; // bob → proxy → alice
    call.expect(180).await;

    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    proxy.forward_response(alice_addr).await;
    let ok = call.expect(200).await;
    assert!(
        get_header(&ok.headers, "record-route").is_some(),
        "200 OK must echo the proxy's Record-Route so alice can build its route set"
    );

    // ACK: alice's route set is now [<proxy;lr>] → ACK carries that Route, RURI
    // stays at bob's contact, and it is sent to the proxy.
    let mut dialog = call.ack().await;
    let fwd_ack = proxy.forward_request(bob_addr).await; // proxy strips its Route, → bob
    bob.receive("ACK").await;
    assert!(
        get_headers(&fwd_ack.headers, "route").is_empty(),
        "proxy must strip its own Route from the in-dialog ACK"
    );

    // BYE: same loose-routing path through the proxy.
    let mut bye = dialog.bye().await;
    let fwd_bye = proxy.forward_request(bob_addr).await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_bye.respond(200, "OK").await;
    proxy.forward_response(alice_addr).await;
    bye.expect(200).await;

    // --- assert the routing on the recording --------------------------------
    let report = h.finish().await;
    let entries = report.entries();
    assert!(entries.iter().all(|e| e.delivered), "all hops delivered");

    // 3 lanes appear (alice, proxy, bob) — the proxy is really in the path.
    let scenario = report.scenario();
    assert_eq!(scenario.lanes.len(), 3, "alice + proxy + bob lanes");

    // The BYE alice put on the wire: Route = <proxy;lr>, Request-URI = bob.
    let alice_bye = parse(&fwd_bye_source(&entries, alice_addr, proxy_addr));
    let SipMessage::Request(bye_req) = alice_bye else { panic!("BYE is a request") };
    assert_eq!(bye_req.method, "BYE");
    assert!(
        bye_req.uri.contains("bob@127.0.0.1:5070"),
        "BYE Request-URI is the remote target (bob), got {}",
        bye_req.uri
    );

    // Sanity on the proxy-forwarded BYE we captured above: Route stripped, RURI
    // unchanged (loose routing — RURI is never the route).
    assert!(get_headers(&fwd_bye.headers, "route").is_empty());
    assert!(fwd_bye.uri.contains("bob@127.0.0.1:5070"));

    // The dialog CSeq still increments correctly through the proxy: BYE = 2.
    assert_eq!(bye_req.cseq.seq, 2);
    assert_eq!(bye_req.cseq.method, "BYE");

    // Render the report (3-lane diagram) and confirm the routing headers are on
    // the wire: a ;lr Record-Route and an in-dialog Route through the proxy.
    let out = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("rr-proxy-call");
    let _ = std::fs::remove_dir_all(&out);
    scenario_harness::report::write_all(&report, &out).unwrap();
    let global = std::fs::read_to_string(out.join("rr-proxy-call.global.txt")).unwrap();
    assert!(global.contains("Record-Route: <sip:127.0.0.1:5080;lr>"));
    assert!(global.contains("Route: <sip:127.0.0.1:5080;lr>"));
}

/// Find the raw BYE alice sent on the wire (alice → proxy), from the recording.
fn fmt_addr(a: std::net::SocketAddr) -> String {
    a.to_string()
}
fn fwd_bye_source(
    entries: &[sip_net::RecordedSipEntry],
    alice: std::net::SocketAddr,
    proxy: std::net::SocketAddr,
) -> Vec<u8> {
    entries
        .iter()
        .find(|e| {
            e.from == alice && e.to == proxy && {
                let m = CustomParser::new().parse(&e.raw);
                matches!(m, Ok(SipMessage::Request(ref r)) if r.method == "BYE")
            }
        })
        .unwrap_or_else(|| panic!("no alice→proxy BYE found (alice={}, proxy={})", fmt_addr(alice), fmt_addr(proxy)))
        .raw
        .clone()
}

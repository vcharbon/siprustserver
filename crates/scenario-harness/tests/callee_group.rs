//! Multi-callee routing — several logical agents on ONE bound socket
//! (newkahneed-022).
//!
//! A call-transfer fabric has three callee-side UAs (Bob the transferor, Charlie
//! the transferee, David the reroute target) and the B2BUA egresses every callee
//! leg to its single ROUTE target — so all three land on ONE address. A plain
//! per-agent socket's single FIFO cannot then disambiguate Charlie's re-INVITE
//! *responses* from Bob's release *requests* on the same queue.
//!
//! [`Harness::callee_group`](scenario_harness::Harness::callee_group) binds one
//! socket and vends the three as distinct logical agents, demultiplexed by the
//! shared R-URI leg-picker (out-of-dialog) and by Call-ID (in-dialog). These
//! tests pin that:
//!
//! * an out-of-dialog INVITE reaches the logical agent whose R-URI user-part
//!   prefix matches — even though the prefixes are *digits*, not the agent names
//!   (the transfer case: name `charlie`, R-URI `231089049…`);
//! * an in-dialog **request** (BYE) follows its dialog's owner by Call-ID;
//! * an in-dialog **response** (the 200 to a callee-originated BYE) follows its
//!   dialog's owner by Call-ID — the case a single FIFO could not route;
//! * a sibling's packet pulled off the shared socket is **stashed** for that
//!   sibling, so the two can be read out of order without a reorder panic;
//! * the same demux works when the leg arrives **via a proxy** (the demux keys
//!   on the R-URI/Call-ID, which are source-address-agnostic).

use std::net::SocketAddr;

use scenario_harness::{Agent, Dialog, Harness, Proxy};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=callee 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

// Prefix-distinct R-URI user-parts — digits, NOT the agent names (the transfer
// shape: the AS rewrites each leg's R-URI to a per-role number).
const BOB_DIGITS: &str = "049231000";
const CHARLIE_DIGITS: &str = "231089049";
const DAVID_DIGITS: &str = "033300712";
const SHARED_ADDR: &str = "127.0.0.1:5070";

fn ruri_for(digits: &str) -> String {
    format!("sip:{digits}@127.0.0.1:5070")
}

/// Establish alice → callee (through the shared socket), asserting the leg was
/// demuxed to `callee` by its R-URI digits. Returns (alice-side dialog,
/// callee-side dialog).
async fn establish(alice: &Agent, callee: &Agent, digits: &str) -> (Dialog, Dialog) {
    let ruri = ruri_for(digits);
    let mut call = alice.invite(callee).with_sdp(OFFER).ruri(&ruri).to(&ruri).send().await;
    let mut uas = callee.receive("INVITE").await;
    assert!(
        uas.request().uri.contains(digits),
        "leg demuxed to the wrong logical agent: R-URI {} should carry {digits}",
        uas.request().uri,
    );
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let alice_dialog = call.ack().await;
    callee.receive("ACK").await;
    let callee_dialog = uas.dialog();
    (alice_dialog, callee_dialog)
}

#[tokio::test(start_paused = true)]
async fn multi_callee_group_demuxes_by_ruri_and_dialog() {
    let h = Harness::new("callee-group-direct").describe(
        "Bob/Charlie/David on ONE bound socket, demuxed by R-URI digits \
         (out-of-dialog) and Call-ID (in-dialog); a callee-originated BYE's 200 \
         is routed by dialog and can be read out of order across siblings.",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let callees = h
        .callee_group(SHARED_ADDR)
        .callee("bob", BOB_DIGITS)
        .callee("charlie", CHARLIE_DIGITS)
        .callee("david", DAVID_DIGITS)
        .build()
        .await;
    let bob = callees.agent("bob");
    let charlie = callees.agent("charlie");
    let david = callees.agent("david");
    assert_eq!(bob.addr(), charlie.addr(), "the three callees share one socket");
    assert_eq!(bob.addr(), david.addr());

    // Three out-of-dialog INVITEs on the one socket, each routed by its R-URI
    // digits to the right logical agent (a mis-route would fail the digit
    // assertion inside `establish`, or time out).
    // The alice-side bob/charlie dialogs are unused after establish — those two
    // calls are torn down by their callee-originated BYE below (alice answers on
    // its own FIFO, not through the dialog handle).
    let (_alice_bob, mut bob_dialog) = establish(&alice, &bob, BOB_DIGITS).await;
    let (_alice_charlie, mut charlie_dialog) = establish(&alice, &charlie, CHARLIE_DIGITS).await;
    let (mut alice_david, _david_dialog) = establish(&alice, &david, DAVID_DIGITS).await;

    // ── in-dialog REQUEST demux: alice BYEs david; the BYE follows David's
    //    dialog by Call-ID on the shared socket, and only David sees it. ──
    let mut d_bye = alice_david.bye().await;
    david.receive("BYE").await.respond(200, "OK").await;
    d_bye.expect(200).await;

    // ── the crossing: Bob AND Charlie each originate a BYE toward alice; alice
    //    200s both. The two 200s land on the shared socket and must be routed
    //    to the right sibling by Call-ID — read OUT OF ORDER (charlie first)
    //    to exercise the per-sibling stash. A single FIFO could not do this. ──
    let mut bob_bye = bob_dialog.bye().await;
    let mut charlie_bye = charlie_dialog.bye().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    alice.receive("BYE").await.respond(200, "OK").await;
    charlie_bye.expect(200).await; // pulls past Bob's 200 (stashed) → Charlie's
    bob_bye.expect(200).await; // Bob's 200 comes back out of the stash

    let report = h.finish().await;
    assert!(report.entries().iter().all(|e| e.delivered), "every hop delivered");
    // Two recorded lanes: alice, and the ONE shared callee socket (bob+charlie+david).
    assert_eq!(report.scenario().lanes.len(), 2, "alice + one shared callee lane");
}

/// Establish alice → proxy → callee (record-routed), asserting the leg was
/// demuxed to `callee` by its R-URI digits from the PROXY's source. Returns the
/// alice-side dialog (whose route set now traverses the proxy).
async fn establish_via_proxy(
    alice: &Agent,
    proxy: &Proxy,
    callee: &Agent,
    digits: &str,
    alice_addr: SocketAddr,
    proxy_addr: SocketAddr,
) -> Dialog {
    let ruri = ruri_for(digits);
    let mut call =
        alice.invite(callee).with_sdp(OFFER).ruri(&ruri).to(&ruri).through(proxy_addr).send().await;
    proxy.forward_request(callee.addr()).await; // proxy → shared socket
    let mut uas = callee.receive("INVITE").await;
    assert!(
        uas.request().uri.contains(digits),
        "proxied leg demuxed to the wrong agent: R-URI {} should carry {digits}",
        uas.request().uri,
    );
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    proxy.forward_response(alice_addr).await;
    call.expect(200).await;
    let alice_dialog = call.ack().await;
    proxy.forward_request(callee.addr()).await; // forward the ACK through the proxy
    callee.receive("ACK").await;
    alice_dialog
}

/// Tear a proxied call down cleanly (alice-originated BYE, forwarded through the
/// proxy; in-dialog demux by Call-ID on the shared socket).
async fn teardown_via_proxy(
    alice_dialog: &mut Dialog,
    proxy: &Proxy,
    callee: &Agent,
    alice_addr: SocketAddr,
) {
    let mut bye = alice_dialog.bye().await;
    proxy.forward_request(callee.addr()).await;
    callee.receive("BYE").await.respond(200, "OK").await;
    proxy.forward_response(alice_addr).await;
    bye.expect(200).await;
}

#[tokio::test(start_paused = true)]
async fn multi_callee_group_demuxes_via_proxy() {
    let h = Harness::new("callee-group-proxy").describe(
        "The same R-URI demux when each leg arrives through a proxy: the picker \
         keys on the R-URI user-part, so a proxied source routes identically.",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let proxy = h.proxy("proxy", "127.0.0.1:5080").await;
    let alice_addr = alice.addr();
    let proxy_addr = proxy.addr();
    let callees = h
        .callee_group(SHARED_ADDR)
        .callee("bob", BOB_DIGITS)
        .callee("charlie", CHARLIE_DIGITS)
        .build()
        .await;
    let bob = callees.agent("bob");
    let charlie = callees.agent("charlie");

    // Two distinct-R-URI legs, each arriving at the ONE shared socket FROM the
    // proxy — still routed to the right logical agent by its R-URI digits.
    let mut alice_bob = establish_via_proxy(&alice, &proxy, &bob, BOB_DIGITS, alice_addr, proxy_addr).await;
    let mut alice_charlie =
        establish_via_proxy(&alice, &proxy, &charlie, CHARLIE_DIGITS, alice_addr, proxy_addr).await;

    teardown_via_proxy(&mut alice_bob, &proxy, &bob, alice_addr).await;
    teardown_via_proxy(&mut alice_charlie, &proxy, &charlie, alice_addr).await;

    let report = h.finish().await;
    assert!(report.entries().iter().all(|e| e.delivered), "every hop delivered");
    // alice + proxy + one shared callee lane.
    assert_eq!(report.scenario().lanes.len(), 3, "alice + proxy + shared callee lane");
}

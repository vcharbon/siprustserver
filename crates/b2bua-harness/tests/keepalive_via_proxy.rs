//! Keepalive OPTIONS flowing through a record-routing front proxy (port of
//! `tests/scenarios/keepalive-via-proxy.ts`, the `keepaliveViaProxy` case).
//!
//! Regression guard for the k8s endurance bug where in-dialog keepalive OPTIONS
//! bypassed the front proxy and every long-hold call was torn down by the
//! keepalive-timeout rule after 15 min.
//!
//! Topology (same as `proxy_b2bua.rs`):
//!
//!   alice :5061 ──▶ proxy :5081 ──▶ b2bua :5091 ──▶ proxy :5081 ──▶ bob :5071
//!
//! With `b2bOutboundProxy` set (the `proxy+b2b` deployment), the b-leg INVITE
//! traverses the proxy, the proxy Record-Routes, the b-leg route set is
//! populated, and subsequent in-dialog OPTIONS flow back through the proxy. Each
//! keepalive OPTIONS therefore arrives carrying ≥2 Via headers (proxy + worker)
//! on *both* legs. We drive two cycles to prove the timer re-arms after a
//! successful round-trip, then tear the call down (the BYE also goes via proxy).
//!
//! Exercises `keepalive` + `absorb-options-200` over the proxy fabric. The source
//! used a 15-min interval; the Rust default is 30 s, so we advance in 30 s steps.

mod common;

use std::time::Duration;

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_message::message_helpers::get_headers;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5061";
const BOB: &str = "127.0.0.1:5071";
const PROXY: &str = "127.0.0.1:5081";
const B2BUA: &str = "127.0.0.1:5091";

/// The Rust default keepalive interval (`KeepaliveActivation.interval_sec`).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn keepalive_options_travels_via_proxy_on_both_legs() {
    // Small one-hop transit (1 ms) so an OPTIONS round-trip through the proxy
    // (4 hops) settles well inside the non-INVITE Timer E (500 ms) — otherwise
    // the proxy's added latency triggers a retransmit and duplicate OPTIONS.
    let h = Harness::with_transit_delay("b2bua-keepalive-via-proxy", 1).describe(
        "keepalive: in-dialog OPTIONS travels via proxy on both legs (regression \
         for k8s endurance teardown).",
    );
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let proxy = common::spawn_lb_proxy(&h, PROXY, "b2bua", B2BUA.parse().unwrap()).await;
    let _b2bua =
        B2buaSut::route_all_to_via_proxy(&h, "b2bua", B2BUA, "127.0.0.1", 5071, "127.0.0.1", 5081)
            .await;

    // ── Call setup (alice → proxy → b2bua → proxy → bob) ─────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── Two keepalive cycles — OPTIONS must traverse the proxy on both legs ──
    // The second cycle confirms the timer rescheduled after the first round-trip.
    for cycle in 0..2 {
        h.advance(KEEPALIVE_INTERVAL).await;

        let mut alice_opts = alice.receive("OPTIONS").await;
        assert!(
            get_headers(&alice_opts.request().headers, "via").len() >= 2,
            "cycle {cycle}: a-leg OPTIONS must carry ≥2 Via (proxy + worker), got {:?}",
            get_headers(&alice_opts.request().headers, "via"),
        );
        alice_opts.respond(200, "OK").await;

        let mut bob_opts = bob.receive("OPTIONS").await;
        assert!(
            get_headers(&bob_opts.request().headers, "via").len() >= 2,
            "cycle {cycle}: b-leg OPTIONS must carry ≥2 Via (proxy + worker), got {:?}",
            get_headers(&bob_opts.request().headers, "via"),
        );
        bob_opts.respond(200, "OK").await;
    }

    // ── Teardown — the in-dialog BYE also travels via the proxy ──────────────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}

//! **Teardown races** — two in-dialog messages cross during/around hang-up.
//! Siblings to `crossing_reinvite_glare` (`reinvite.rs`) and the 200/CANCEL
//! crossing (`cancel_200_crossing.rs`). The invariant every one of these pins:
//! a racing teardown must reap the call EXACTLY ONCE and release its limiter
//! hold EXACTLY ONCE — never a double-decrement, never a stranded slot. A real
//! `LimiterServer` is wired and `current_total` must land on 0.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{settle_until, B2buaSut};
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

async fn serve_limiter(net: &SimulatedHttpNetwork) -> (Arc<WindowStore>, Box<dyn HttpServerHandle>) {
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = net.serve(laddr(), server).await.unwrap();
    (store, handle)
}

fn limiter_client(net: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(
        Arc::new(net.clone()),
        laddr(),
        Duration::from_millis(150),
    ))
}

fn route_limited(host: &str, port: u16, id: &str, limit: i64) -> Arc<dyn CallDecisionEngine> {
    let host = host.to_string();
    let id = id.to_string();
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to(&host, port);
                r.call_limiter = vec![CallLimiterEntry { id: id.clone(), limit }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// **BYE/BYE glare.** Both parties hang up at the same instant: alice's BYE and
/// bob's BYE cross at the B2BUA. The B2BUA answers each BYE (200) and tears the
/// peer down; the second BYE lands on the already-terminating call. The call
/// must reap once and the limiter hold must drop to 0 exactly once (a
/// double-decrement here would corrupt the shared trunk counter).
#[tokio::test(start_paused = true)]
async fn bye_bye_glare_reaps_once_and_releases_the_limiter_once() {
    let h = Harness::new("b2bua-race-bye-bye");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    let decision = route_limited("127.0.0.1", 5070, "trunk-A", 1);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let (mut alice_dialog, mut bob_dialog) =
        establish_call_both_sides(&alice, &bob, &b2bua, &store).await;

    // ── both hang up at once ──────────────────────────────────────────────────
    let _a_bye = alice_dialog.bye().await;
    let _b_bye = bob_dialog.bye().await;

    // Let both BYEs, their 200s and the peer-teardown BYEs all land, then drain
    // each UA's queue (we assert on the SUT's reap + limiter, not on which BYE
    // won the glare). Advance past the 32 s TerminatingTimeout so any BYE the
    // drained UA never answered is force-resolved.
    h.advance(Duration::from_secs(1)).await;
    alice.drain().await;
    bob.drain().await;
    h.advance(Duration::from_secs(33)).await;

    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "limiter released exactly once on the BYE glare");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// **Re-INVITE crossing a BYE.** Alice fires a re-INVITE just as bob hangs up;
/// the re-INVITE and the BYE cross at the B2BUA. The BYE wins (you cannot
/// renegotiate a dialog that is being torn down): the call terminates and the
/// in-flight re-INVITE is abandoned. The call must reap and the limiter release.
#[tokio::test(start_paused = true)]
async fn reinvite_crossing_bye_terminates_and_releases_the_limiter() {
    let h = Harness::new("b2bua-race-reinvite-bye");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    let decision = route_limited("127.0.0.1", 5071, "trunk-A", 1);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;

    let (mut alice_dialog, mut bob_dialog) =
        establish_call_both_sides(&alice, &bob, &b2bua, &store).await;

    // ── alice re-INVITEs as bob BYEs (crossing) ───────────────────────────────
    let _reinv = alice_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let _b_bye = bob_dialog.bye().await;

    // Drain the crossing traffic (relayed re-INVITE, BYEs, the abandoned
    // re-INVITE's response) and force-resolve any unanswered BYE past the 32 s
    // TerminatingTimeout. The invariant under test is the SUT's clean reap.
    h.advance(Duration::from_secs(1)).await;
    alice.drain().await;
    bob.drain().await;
    h.advance(Duration::from_secs(33)).await;

    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "limiter released after the re-INVITE/BYE crossing");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// Establish a confirmed call and hand back BOTH dialogs (alice's UAC side and
/// bob's UAS side), asserting the limiter hold was taken. Mirrors the inline
/// setup the crossing tests do by hand.
async fn establish_call_both_sides(
    alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    b2bua: &B2buaSut,
    store: &Arc<WindowStore>,
) -> (scenario_harness::Dialog, scenario_harness::Dialog) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(b2bua_harness::ANSWER_SDP).await;
    call.expect(200).await;
    let alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let bob_dialog = uas.dialog();
    assert_eq!(store.stats().current_total, 1, "established call holds one limiter slot");
    (alice_dialog, bob_dialog)
}

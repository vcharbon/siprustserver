//! Long ringing PAST the default 158 s initial-INVITE bound, under HA
//!: the raised `invite_txn_timeout_sec` must hold through the
//! simulated cluster — worker crash, pristine reboot, reclaim — and the
//! ledger-replicated `SetupTimeout` must fire at its ORIGINAL absolute
//! deadline on the reclaimed node (neither early nor extended), giving the
//! caller its final and settling every obligation (one CDR, limiter zero).
//!
//! The worker tune rides [`FailoverHarness::with_worker_tune`], which re-applies
//! on every spawn AND reboot — so the reborn primary carries the same raised
//! bound its dead incarnation armed the call under.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use failover_harness::{
    assert_call_fully_over, cookie_field, FailoverHarness, ReplicatedB2buaSut, WorkerHealth,
};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The raised transaction bound under test (seconds).
const INVITE_TXN_SEC: i64 = 400;
/// The app setup deadline (seconds): 180 s — the PSTN ring-supervision value
/// the 158 s const could never admit — and strictly inside the LB's cancel-LRU
/// TTL (158 s + Timer H ≈ 190 s, `cancel_lru.rs`), so the give-up CANCEL still
/// follows the INVITE hop to the callee. A deadline past that TTL degrades to
/// an orphaned CANCEL at the LB; the deferred proxy follow-up owns raising the
/// TTL alongside `B2BUA_INVITE_TXN_TIMEOUT_SEC`.
const SETUP_SEC: i64 = 180;

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

fn limiter_client(http: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr(), Duration::from_millis(150)))
}

/// Route to bob through the outbound proxy with a `limit:1` hold — the
/// limiter_ha decision shape, so the release is observable at the shared store.
fn limited_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![CallLimiterEntry { id: "trunk-A".into(), limit: 1 }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// Crash the primary mid-ring, reboot + reclaim, then keep ringing PAST the
/// old 158 s const (the raised bound holds on the reclaimed node — neither the
/// txn layer nor the restored `SetupTimeout` fires early), and cross the
/// ORIGINAL 180 s deadline: the CANCEL reaches bob through the LB's INVITE-hop
/// memory, the caller takes its 408, and the call is fully over cluster-wide.
#[tokio::test(start_paused = true)]
async fn raised_bound_holds_a_ring_across_crash_reboot_reclaim_until_the_original_deadline() {
    let mut fh = FailoverHarness::new("long-ring-ha-crash-reclaim", &["b1", "b2"])
        .with_worker_tune(|c| {
            c.invite_txn_timeout_sec = INVITE_TXN_SEC;
            c.setup_timeout_sec = SETUP_SEC;
        });
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    // Shared limiter server on its own simulated HTTP fabric (survives crashes).
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &["b2"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            limited_decision(),
            limiter_client(&http),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2",
            "b2",
            B2,
            &["b1"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            limited_decision(),
            limiter_client(&http),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready");

    // ── Ringing setup: bob rings and never answers ────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let primary_ord = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Replicate the in-setup call (hold + the route-time SetupTimeout ledger
    // entry) primary → backup before anything else happens.
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(store.stats().current_total, 1, "in-setup call holds its limiter slot");

    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let call_ref = backup
        .scan_one_backed_up(&primary_ord)
        .await
        .expect("the ringing call replicated to the backup");

    // ── 100 s of quiet ringing, then the mid-ring crash ───────────────────────
    fh.advance(Duration::from_secs(99)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "no transaction-layer CANCEL while ringing inside the raised bound",
    );
    assert_eq!(primary.active_calls(), 1, "the long-ringing call is still up");
    assert_eq!(store.stats().current_total, 1, "the hold is still pinned by the ring");

    // ── Crash + reboot-pristine + reclaim (the endurance kill_worker shape) ───
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    let new_addr = primary.reboot().await;
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary re-hydrated from the backup");
    proxy.set_address(&primary_ord, new_addr);
    fh.note_worker_rebound(&primary_ord, new_addr);
    proxy.set_health(&primary_ord, WorkerHealth::Alive);

    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(
        primary.active_calls(),
        1,
        "the reboot reclaim re-materialised the still-ringing call",
    );
    assert_eq!(store.stats().current_total, 1, "the hold rides the reclaim");

    // ── PAST the old 158 s const on the RECLAIMED node, and not early: at 174 s
    // (6 s before the ORIGINAL deadline) the raised bound still holds the ring —
    // no transaction-layer CANCEL, no early fire of the restored SetupTimeout. ─
    let to_174 = 174_000_i64 - fh.now_ms();
    assert!(to_174 > 0, "reboot+reclaim completed inside the ring window");
    fh.advance(Duration::from_millis(to_174 as u64)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "the raised bound holds past 158 s and the re-anchored SetupTimeout \
         must not fire early",
    );
    assert_eq!(store.stats().current_total, 1, "hold intact just before the deadline");

    // ── Nor extended: crossing the ORIGINAL 180 s mark trips the restored
    // deadline. The b-leg CANCEL lands inside the LB's cancel-LRU TTL, so it
    // follows the INVITE hop to bob; the caller takes its 408 (and ACKs it).
    // Bob is the dead-peer shape from here — he never answers the CANCEL —
    // so the SUT resolves the `Cancelling` b-leg at its own ~32 s terminating
    // backstop (the `limiter_ha` setup-stall treatment).
    fh.advance(Duration::from_secs(7)).await;
    bob.receive("CANCEL").await;
    let final_resp = call.expect(408).await;
    assert_eq!(final_resp.status(), 408, "caller's INVITE resolves at the restored setup deadline",);
    // Flush alice's §17.1.1.3 ACK for the 408 the one hop to the proxy.
    // (`uas` — bob's ringing server txn — stays in scope untouched: the
    // dead-peer callee never answers the CANCEL.)
    fh.advance(Duration::from_millis(10)).await;

    // ── Fully over cluster-wide: the terminating backstop (~32 s) settles the
    // held b-leg, then exactly one CDR, limiter drained, no trace anywhere. ──
    let mut released = false;
    for _ in 0..250 {
        fh.advance(Duration::from_millis(200)).await;
        if store.stats().current_total == 0 {
            released = true;
            break;
        }
    }
    assert!(
        released,
        "the reclaimed call must release its limiter hold shortly after the restored \
         deadline (SetupTimeout + the terminating safety backstop)",
    );
    // Let the terminal tombstone replicate so the backup sheds its replica
    // before the strict cluster-wide "no trace anywhere" assertion.
    for _ in 0..50 {
        fh.advance(Duration::from_millis(200)).await;
        if !w_b1.holds_any_trace(&call_ref).await && !w_b2.holds_any_trace(&call_ref).await {
            break;
        }
    }
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
    drop(proxy);
}

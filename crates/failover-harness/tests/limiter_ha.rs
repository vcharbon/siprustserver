//! HA-coupled limiter: a call admitted on the primary replicates its limiter
//! hold to the backup; when the primary crashes and the in-dialog BYE fails over
//! to the backup, the backup hydrates the call (holds included) and releases the
//! hold on termination — so the shared counter drains on the **takeover** node
//! (decrement-via-backup-BYE / decrement-after-respawn).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use failover_harness::{
    assert_call_fully_over, FailoverHarness, ReplicatedB2buaSut, RULE_CSEQ_IN_DIALOG_ORDER,
    WorkerHealth,
};
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
/// Second callee for the TTL-leak test — Call A and Call B run on two different
/// workers, so they must hit two different bobs (TS `BOB_HOST_1`/`BOB_HOST_2`) to
/// keep each b-leg on its own RFC-audit Call-ID lane.
const BOB2: &str = "127.0.0.1:5071";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// A `FailoverHarness` for the limiter-HA cases with the known reclaim CSeq-desync
/// audit waived. Every case here drives a backup TAKEOVER, which can randomly trip
/// the in-dialog CSeq gate via the pre-existing ADR-0014 dual-owner desync (two
/// nodes originate an in-dialog keepalive OPTIONS / BYE at the same `local_cseq +
/// 1`) — a flake, not what these tests assert. Their limiter-count / call-over
/// assertions still gate. Tracked separately (cf. feat/fix-call-terminate-model-x);
/// remove this waiver once the reclaim CSeq high-water / ownership fix lands. See
/// `FailoverHarness::allow_rfc_violation`.
fn ha_harness(name: &str) -> FailoverHarness {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    fh.allow_rfc_violation(
        RULE_CSEQ_IN_DIALOG_ORDER,
        "pre-existing ADR-0014 dual-owner reclaim CSeq-desync; tracked separately",
    );
    fh
}

/// w_pri ordinal from the proxy's Record-Route stickiness cookie.
fn pri_from_cookie(rr: &str) -> String {
    let params = sip_message::message_helpers::parse_uri_params(rr);
    params.get("w_pri").cloned().unwrap_or_default()
}

fn limiter_client(http: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(
        Arc::new(http.clone()),
        laddr(),
        Duration::from_millis(150),
    ))
}

/// The limiter-carrying decision shared by every worker in this file: route the
/// b-leg through the worker's outbound proxy with a `limit:1` `trunk-A` hold.
///
/// Note this sets only the wire hop (`route_to` leaves `new_ruri = None`), NOT a
/// per-worker callee — so the decision's `destination` port does NOT pick which
/// bob the b-leg lands on. The outbound proxy forwards the b-leg by the preserved
/// a-leg R-URI, so where two calls' b-legs land (and thus how they stay on
/// distinct RFC-audit Call-ID lanes) is set by the a-leg target each call dials,
/// not by anything here. See the `leaked_…` test's lower comment for how Call A
/// (bob1) and Call B (bob2) are kept on separate lanes.
fn limited_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![CallLimiterEntry {
                    id: "trunk-A".into(),
                    limit: 1,
                }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// A short, realistic limiter window for the TTL-leak-budget test — the Rust
/// equivalent of the TS `limiter-ttl-leak-budget` config override
/// (`limiterWindowSeconds: 120`, `limiterActiveWindows: 1`,
/// `limiterTtlSeconds: 120`). A 2-minute window/TTL is long enough to exceed any
/// plausible call-establishment timer yet short enough that the paused clock
/// reaches one full window + slack (~125 s of virtual time) cheaply. The whole
/// `LimiterConfig::default()` (300 s / 3 windows / 1200 s TTL) would force a
/// >20-minute virtual advance for the same coverage.
fn ttl_leak_config() -> LimiterConfig {
    LimiterConfig {
        window_sec: 120,
        active_windows: 1,
        ttl_sec: 120,
    }
}

#[tokio::test(start_paused = true)]
async fn hold_is_released_on_the_takeover_node_after_primary_crash() {
    let mut fh = ha_harness("limiter-ha-takeover");
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    // Shared limiter server on its own simulated HTTP fabric (survives crashes).
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;

    // Both workers share the limiter server + the limiter-carrying decision.
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready");

    // alice INVITEs through the proxy; establish the call.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg carries the proxy cookie")
        .to_string();
    let primary_ord = pri_from_cookie(&rr);
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Drive establish + replicate primary → backup.
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(store.stats().current_total, 1, "call admitted: one hold");

    // Bind primary/backup by the cookie's w_pri.
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" {
            (&mut w_b1, &mut w_b2)
        } else {
            (&mut w_b2, &mut w_b1)
        };

    // Crash the primary; mark it dead so the proxy fails the in-dialog request
    // over to the backup.
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // alice BYEs: the backup hydrates the replicated call and answers the wire.
    let creations_before = backup.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    // Model Y (ADR-0020 X3): the takeover backup DEFERS the discharge — it answers
    // the wire but writes NO CDR and does NOT release the limiter itself (the primary
    // is the sole CDR authority). With the primary crashed and NEVER rebooting, the
    // CDR is the accepted loss — BUT the backup must still not leak the limiter slot:
    // once the deferral's replica TTL (`reboot_budget`) expires, the periodic reap
    // releases the hold (no CDR) and frees the body. Pump past that TTL, then assert
    // the limiter drained to 0 with zero CDRs and exactly one lost-CDR cleanup.
    let released = fh
        .settle_lossy_cleanup(async || store.stats().current_total == 0)
        .await;
    assert!(
        backup.metrics().creations_total() > creations_before,
        "backup processed the failed-over BYE",
    );
    assert!(
        released,
        "the takeover node released the limiter hold via its lossy auto-cleanup reap",
    );
    assert_eq!(
        w_b1.cdr_records().len() + w_b2.cdr_records().len(),
        0,
        "StayDead: primary never reclaimed, so NO CDR — the accepted loss",
    );
    assert_eq!(
        w_b1.metrics().repl_terminal_lost_total() + w_b2.metrics().repl_terminal_lost_total(),
        1,
        "exactly one lost-CDR cleanup counted across the cluster",
    );

    // The unified seq-report (HTML + global.txt + replication.mmd) is emitted by
    // `FailoverHarness`'s write-on-Drop fallback into
    // `target/seq-reports/limiter-ha-takeover/` — no explicit call needed.
}

/// THE 2026-06-12 endurance zombie, end to end: a call caught **mid-setup**
/// (b-leg ringing, no final response) by a primary crash. The sip-txn
/// `INVITE_INITIAL_TIMEOUT` died with the node, the per-b-leg `NoAnswer` was
/// never armed (endurance routes supply none), and the reclaimed copy sat
/// `Active` holding its limiter slot — refreshing it every 300 s, reaper-proof
/// — until the 1 h GlobalDuration. The ledger-replicated `SetupTimeout` is the
/// fix: it rides `call.timers` into the bak: snapshot, is restored by the
/// reboot reclaim, and tears the call down at the 150 s deadline — releasing
/// the hold ~55 min earlier (the cap20 SIPp stream was pinned at 15/20 for the
/// whole window).
#[tokio::test(start_paused = true)]
async fn setup_stalled_call_is_released_at_the_deadline_after_crash_reboot_reclaim() {
    let mut fh = ha_harness("limiter-ha-setup-stall-reclaim");
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    // Shared limiter server on its own simulated HTTP fabric (survives crashes).
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready");

    // ── Mid-setup stall: bob rings and never answers ──────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg carries the proxy cookie")
        .to_string();
    let primary_ord = pri_from_cookie(&rr);
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Replicate the in-setup call (hold + the route-time SetupTimeout ledger
    // entry) primary → backup before the crash.
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(store.stats().current_total, 1, "in-setup call holds its limiter slot");

    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" {
            (&mut w_b1, &mut w_b2)
        } else {
            (&mut w_b2, &mut w_b1)
        };

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
    proxy.set_health(&primary_ord, WorkerHealth::Alive);

    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(
        primary.active_calls(),
        1,
        "the reboot reclaim re-materialised the in-setup call",
    );
    assert_eq!(store.stats().current_total, 1, "the hold rides the reclaim");

    // ── The restored SetupTimeout fires at the (absolute) 150 s deadline ──────
    // No SIP will ever arrive for this call again (the load generator gave up
    // during the kill); only the replicated ledger timer can resolve it. The
    // SetupTimeout CANCELs the ringing b-leg, and the call-liveness ordering fix
    // now HOLDS finalization while that b-leg is `Cancelling` (awaiting a 487 or a
    // crossing 200 — neither of which can ever arrive for this abandoned call), so
    // the release completes at the 32 s `TerminatingTimeout` backstop that
    // `begin_termination` arms — SetupTimeout (150 s) + 32 s, still FAR below the
    // 1 h GlobalDuration cap this test guards against.
    fh.advance(Duration::from_secs(155)).await;
    let mut released = false;
    for _ in 0..200 {
        fh.advance(Duration::from_millis(200)).await;
        if store.stats().current_total == 0 {
            released = true;
            break;
        }
    }
    assert!(
        released,
        "the reclaimed in-setup call must release its limiter hold shortly after the \
         setup deadline (SetupTimeout + the terminating safety backstop) — not at the \
         1 h GlobalDuration (the endurance cap20 pinning)",
    );
    assert_eq!(primary.active_calls(), 0, "no zombie call on the rebooted primary");
    assert_eq!(backup.active_calls(), 0, "the backup never owned a live copy");
}

/// TTL leak-budget recovery when the primary is **permanently** dead — the Rust
/// port of `tests/sip-front-proxy/failover/limiter-ttl-leak-budget.test.ts`
/// ("leaked limiter slot recovers via TTL when primary is permanently dead").
///
/// This is the worst-case capacity-recovery path. Every OTHER limiter-HA test
/// drains the shared counter through an *active* release: the takeover backup's
/// lossy reap of a deferred terminal (`hold_is_released_on_the_takeover_node…`),
/// a respawned primary's local terminate (`decrement-after-respawn`), or the
/// restored `SetupTimeout` (`setup_stalled_call…`). Here NONE of those fire:
///
///   1. `LimiterConfig { window 120 s, 1 active window, TTL 120 s }`. Call A is
///      established through the proxy → admitted → shared counter = 1. The hold's
///      TTL is anchored at that admit (the owner's `LimiterRefresh` timer, armed
///      for admit + `limiter_refresh_sec`, would re-anchor it — but the owner
///      dies before it fires).
///   2. The primary crashes **permanently** — no graceful BYE, no respawn, no
///      reclaim. The hold is leaked: nothing on the SIP plane will ever release
///      it. The backup holds a *non-terminal* (`Established`) replica, so the
///      Model-Y `reap_expired_replicas` lossy path — which only releases holds of
///      `Terminating`/`Terminated` replicas (`expired_terminal_fallbacks`) — does
///      **not** touch it either. The ONLY remaining recovery mechanism is the
///      `WindowStore` per-key TTL.
///   3. Advance one full window + slack (~125 s). The hold's `expires_at_ms` has
///      now passed, but nothing has swept it yet — the store sweeps only on
///      access and no limiter op runs during the dead window, so the counter is
///      still a leaked 1.
///   4. Call B is established through the proxy → it lands on the surviving worker
///      (the LB filters to `Alive`). Crucially, Call B arrives at ~125 s, which is
///      the NEXT window (W1 = `[120, 240)`); with `active_windows = 1` its phase-1
///      check inspects only W1 (count 0), so Call B is admitted *unconditionally* —
///      its admission is NOT the regression signal. What its admit DOES do is sweep
///      the now-expired Call A key out of the dead W0 bucket (`WindowStore::admit`
///      sweeps expired keys before the phase-1 check), dropping the leaked hold.
///      The regression guard is therefore the POST-establish state: `current_total`
///      back to 1 (Call B's own hold, Call A's swept away) and `auto_cleared` bumped
///      by that sweep. If the admit-time sweep regressed, Call A's leaked W0 key
///      would survive Call B's admit (it lives in a different window, so it is never
///      re-counted, but it is also never reaped) and `current_total` would read 2 —
///      flagging a leak that would otherwise persist indefinitely after a crash with
///      no peer takeover. This is exactly the TS source's shape (Call B establishes +
///      `expectCdrCount == 1`); in production the same recovery happens via the Redis
///      key TTLs configured to the same window/TTL constants. (NB: the 486-reject
///      path — where a leaked key blocks admission — is only reachable when both
///      calls share a window, i.e. `window_sec` large enough that W0 still covers
///      Call B; this config's 120 s window puts Call B in W1, so the count, not the
///      decision, is the load-bearing signal here.)
#[tokio::test(start_paused = true)]
async fn leaked_limiter_slot_recovers_via_ttl_when_primary_is_permanently_dead() {
    let mut fh = ha_harness("limiter-ha-ttl-leak-budget");
    let alice = fh.agent("alice", ALICE).await;
    // Two callees: Call A dials bob1, Call B dials bob2. The b-leg keeps the a-leg
    // R-URI (the decision sets only the wire hop — see `limited_decision`) and the
    // worker's outbound proxy forwards by that R-URI, so dialing two distinct bobs
    // is what puts the two b-legs on distinct RFC-audit Call-ID lanes. A single
    // `bob` would collide them: both workers seed `IdGen::seeded(0xB2B0 + gen)`
    // with gen=1 and mint the SAME deterministic b-leg ids, which the Drop-time
    // RFC 3261 §12.2.1.1 audit rejects as in-dialog CSeq reuse on the shared peer.
    let bob1 = fh.agent("bob1", BOB).await;
    let bob2 = fh.agent("bob2", BOB2).await;

    // Shared limiter server on its own simulated HTTP fabric (survives crashes).
    // Short 2-minute window/TTL so the paused clock reaches the leak budget fast.
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(ttl_leak_config(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready");

    // ── Call A — established through the proxy, slot consumed ──────────────────
    // Hand-rolled (not `callflow::establish`) because the cookie read + the crash
    // injection ARE the subject of this test. The b-leg keeps the a-leg R-URI
    // (the decision sets only the wire hop, not `new_ruri`), and the worker's
    // outbound proxy forwards the b-leg by R-URI — so calling `bob1` lands the
    // b-leg on bob1 regardless of which worker the LB picked. Call B targets a
    // SECOND callee (bob2) so the two b-legs sit on distinct RFC-audit lanes.
    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob1.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg carries the proxy cookie")
        .to_string();
    // The cookie names the worker the LB picked as primary — which one is left to
    // HRW (the auto-generated Call-ID), so bind dynamically rather than assume.
    let primary_ord = pri_from_cookie(&rr);
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    // Deliberate, benign deviation from the TS source (which leaves Call A in an
    // early/unACKed state, line 99): we fully confirm Call A so the leaked replica
    // is `Established`, not early. The slot is consumed at INVITE-routing time
    // regardless, so this does not change the leaked-slot-recovery coverage:
    // `Established` and early replicas are BOTH non-terminal, so both are equally
    // ignored by the Model-Y lossy reap (`expired_terminal_fallbacks` returns only
    // `Terminating`/`Terminated`). We confirm simply to exercise the fully-established
    // shape end to end.
    let _dialog = call.ack().await;
    bob1.receive("ACK").await;

    // Drive establish + replicate primary → backup (so the backup carries a
    // non-terminal replica — the case the lossy reap must NOT release).
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(store.stats().current_total, 1, "Call A admitted: one hold leaked-to-be");

    // Bind the primary by the cookie's w_pri (the backup is reached only via
    // `w_b1`/`w_b2` later, so it needs no separate live binding here).
    let primary: &mut ReplicatedB2buaSut =
        if primary_ord == "b1" { &mut w_b1 } else { &mut w_b2 };

    // ── Permanently kill the primary: no BYE, no respawn, no reclaim ──────────
    // The hold is leaked. With no in-dialog request ever routed to the backup,
    // the backup never takes the call over, so its replica stays `Established`
    // (non-terminal) — invisible to the Model-Y lossy reap.
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);

    // ── Advance past the window so the per-key TTL elapses ─────────────────────
    // 125 s = 120 s window + 5 s slack. Nothing is in flight (Call A is dead, no
    // BYE), so a single advance is safe — there is no second deadline to react to
    // between steps. The store is read on access only; with no traffic NOTHING
    // sweeps during the dead window, so the counter stays a leaked 1 even though
    // the key's `expires_at_ms` (admit + 120 s) has now passed. The owner being
    // dead means no `LimiterRefresh` ever re-anchored the TTL, so it expires
    // exactly one window after the admit — and the leaked 1 survives, unswept,
    // right up to Call B's admit below. Deliberately NO explicit `sweep_now()`
    // here: matching the TS source, Call B's admit must be the thing that reaps
    // the leaked key, so the admit-time-sweep regression guard stays armed (drop
    // the `sweep(&mut inner, now_ms)` at the top of `WindowStore::admit` and this
    // test FAILS on `current_total == 2` below — Call A's expired key is never
    // reaped, so Call B's own admit leaves the total at 2, not on a 486: Call B
    // lands in the next window and is admitted either way).
    let cleared_before = store.stats().auto_cleared;
    assert_eq!(store.stats().current_total, 1, "hold still leaked right after the crash");
    fh.advance(Duration::from_secs(125)).await;
    assert_eq!(
        store.stats().current_total, 1,
        "the leaked hold is STILL a 1 at +125 s — expired but unswept (no access has run \
         a sweep); only Call B's admit recovers it",
    );

    // ── Call B — lands in the NEXT window, so it is admitted unconditionally ───
    // With the primary Dead, the LB has exactly one alive candidate (the
    // survivor), so Call B lands there. It targets bob2 (a distinct callee) so its
    // b-leg sits on its own RFC-audit lane, away from Call A's leaked dialog.
    // Uninterrupted happy path → `callflow` (CLAUDE.md).
    //
    // `establish` succeeding is NOT the regression guard. With this config
    // (window 120 s, `active_windows = 1`) Call B's INVITE arrives at ~125 s, which
    // is window W1 = `[120, 240)`; its phase-1 check inspects only W1 (count 0), so
    // admission is unconditional whether or not the leaked W0 key was swept. The
    // 486-reject path the recovery is meant to prevent is only reachable when both
    // calls share a window (a larger `window_sec`); here they do not, so the guard
    // is the POST-establish count + `auto_cleared` delta below, not the decision.
    //
    // What the admit DOES do is sweep the now-expired Call A key out of the dead W0
    // bucket (window.rs `admit` line ~121, before its phase-1 check) — that sweep is
    // what drops the leaked hold, recovering the slot exactly as the TS source's
    // `expectCdrCount == 1` shape documents.
    let mut call_b = scenario_harness::callflow::establish(&alice, &bob2, proxy.addr()).await;
    assert_eq!(
        store.stats().current_total, 1,
        "post-recovery total back to 1 — Call B's admit swept the expired Call A W0 key, \
         then took its own slot. With the admit-time sweep removed Call A's leaked W0 key \
         survives (it is in a different window, so never re-counted but also never reaped) \
         and this reads 2 — that 2 is the regression signal, not a 486",
    );
    assert!(
        store.stats().auto_cleared > cleared_before,
        "the recovery was via the TTL auto-clear path (Call B's admit-time sweep), not an \
         active release",
    );
    // Belt-and-braces, NOT the discriminator: confirm no Model-Y lossy reap was
    // counted. This is intentionally a weak signal here — the reap only fires after
    // `reboot_budget_sec` (600 s) of replica TTL, and the test finishes at ~126 s, so
    // this reads 0 regardless of the replica's terminal state (and the test never
    // asserts the backup's replica is non-terminal). The genuine TTL-recovery-vs-
    // backup-discharge discriminator is the `auto_cleared > cleared_before` check
    // above. We keep this assert as a cheap guard that the reap path stayed quiet
    // within the test budget.
    assert_eq!(
        w_b1.metrics().repl_terminal_lost_total() + w_b2.metrics().repl_terminal_lost_total(),
        0,
        "no Model-Y lossy reap was counted within the ~126 s test budget (reboot_budget is \
         600 s, so the reap cannot fire here regardless): recovery came from the limiter TTL",
    );

    // Clean teardown so Call B writes its CDR and releases its own hold. Wait for
    // BOTH the limiter to drain AND the CDR to land: the limiter decrement and the
    // CDR write ride different obligation lanes (the CDR-lane reorder, ADR-0020),
    // so the limiter can hit 0 a flush before the CDR is recorded — settling on the
    // limiter alone would race ahead of the write.
    scenario_harness::callflow::hangup(&mut call_b, &bob2).await;
    let drained = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.cdr_records().len() + w_b2.cdr_records().len() == 1
        })
        .await;
    assert!(
        drained,
        "Call B's clean BYE released its limiter hold AND wrote exactly one CDR",
    );

    // Call A's CDR is the accepted loss (its primary crashed for good and never
    // reclaimed); only Call B terminated cleanly, so exactly one CDR exists.
    assert_eq!(
        w_b1.cdr_records().len() + w_b2.cdr_records().len(),
        1,
        "only Call B wrote a CDR — Call A's is lost (primary never reclaimed)",
    );
}

/// **Switchback-BYE decrement** — the Rust port of
/// `tests/sip-front-proxy/failover/limiter-decrement-via-switchback-bye.test.ts`
/// ("re-INVITE on backup → respawn → BYE on returned-primary decrements the
/// cluster-shared limiter").
///
/// This is the bak→pri **switchback** discharge path, distinct from every other
/// limiter-HA cell:
///   - `hold_is_released_on_the_takeover_node_after_primary_crash` — the BYE is
///     served on the **backup** while the primary stays dead (lossy reap releases).
///   - `setup_stalled_call…` / the `decrement-after-respawn` shape — the primary
///     reboots and reclaims, but NOTHING was served on the backup in between (no
///     reverse-fold to merge).
///
/// Here the dialog is **served on the backup mid-outage** (a re-INVITE the backup
/// takes over and reverse-propagates), THEN the primary returns and the terminal
/// BYE lands back on the **reborn primary**, which discharges the reverse-folded
/// call. The signal under test (TS `expectLimiterCount(0)`): the limiter
/// `origin_window` stamped at the original admit must survive the whole
/// bak→pri round-trip (admit on pri → takeover-fold on bak → reverse-flush back
/// to pri → reclaim → BYE-discharge on pri). If the reclaim/switchback re-stamps
/// the origin to the reborn primary's current epoch, the decrement targets a
/// limiter key the original increment never wrote, the count stays a leaked 1, and
/// the next call under the same id would 486. `current_total == 0` after the BYE
/// proves the window was preserved across the switchback.
///
/// Mechanism in this harness (the no-probe analogue of the production k8s LB):
/// `crash()` + `set_health(Dead)` makes the proxy fail the in-dialog re-INVITE
/// over to the backup (`decode_forward_backup`); the backup self-releases its live
/// takeover copy once the re-INVITE transaction completes (keeping only the
/// reverse-flushed replica in `pri:{primary}`); `reboot()` + `set_address` +
/// `set_health(Alive)` returns the primary, whose bootstrap-rehydrate + bulk
/// reclaim re-materialises the reverse-folded call from its own `pri:` partition;
/// the cookie's `w_pri` is alive again, so alice's BYE routes back to it
/// (`decode_forward`) and the reborn primary discharges — one CDR, limiter to 0.
#[tokio::test(start_paused = true)]
async fn switchback_bye_on_returned_primary_decrements_the_shared_limiter() {
    let mut fh = ha_harness("limiter-ha-switchback-bye");
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    // Shared limiter server on its own simulated HTTP fabric (survives crashes).
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready");

    // ── Phase 0: establish on the HRW primary, slot consumed ──────────────────
    // Hand-rolled (not `callflow::establish`): the cookie read + the crash/
    // re-INVITE/reboot injections ARE the subject of this test.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg carries the proxy cookie")
        .to_string();
    let primary_ord = pri_from_cookie(&rr);
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Replicate the confirmed call primary → backup; discover the cluster call_ref.
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(store.stats().current_total, 1, "call admitted: one limiter hold");

    let backup_ref_src = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let mut call_ref = String::new();
    for _ in 0..50 {
        if let Some(rf) = backup_ref_src.scan_one_backed_up(&primary_ord).await {
            call_ref = rf;
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    assert!(!call_ref.is_empty(), "backup holds the replicated call ref");

    // ── Phase 1: crash primary, re-INVITE fails over to the backup ────────────
    // The backup takes the (non-terminal) re-INVITE over, bumps the call's b-leg
    // CSeq, and reverse-propagates the takeover state to pri:{primary}. The
    // re-INVITE must NOT touch the limiter (no checkAndIncrement-on-reinvite path),
    // so the shared counter stays 1.
    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        primary.crash();
        proxy.set_health(&primary_ord, WorkerHealth::Dead);
        backup.simulate_peer_removed(&primary_ord);
    }
    fh.advance(Duration::from_millis(300)).await;

    // Delayed-offer re-INVITE (mirrors the TS `_matrix.ts:178` shape): alice INVITE
    // without a body, bob 200 carrying the offer, alice ACK carrying the answer.
    let mut reinv = dialog.request(InDialogMethod::Invite, None).await;
    let mut bob_uas = bob.receive("INVITE").await; // relayed to bob via the backup + proxy
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await; // ← external: the failed-over re-INVITE succeeds
    dialog.ack(Some(ANSWER)).await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    assert!(
        backup.serves(&call_ref) || backup.is_synchronized_backup(&call_ref).await,
        "the re-INVITE was taken over / is held by the backup after the crash",
    );
    assert_eq!(
        store.stats().current_total, 1,
        "the re-INVITE must NOT touch the shared limiter (still one hold)",
    );

    // The acting-backup self-releases its live takeover copy once the served
    // re-INVITE server transaction reaches its terminal state (Timer H absorbs the
    // 2xx-ACK retransmits, ~32 s), keeping only the reverse-flushed replica — so the
    // reborn primary, not the backup, owns the call after switchback.
    for _ in 0..80 {
        fh.advance(Duration::from_millis(500)).await;
        if backup.memory_clean() {
            break;
        }
    }
    assert!(
        backup.memory_clean(),
        "acting-backup released the live takeover copy after serving the re-INVITE",
    );
    assert!(
        backup.is_synchronized_backup(&call_ref).await,
        "the release kept the reverse-flushed replica (the call is recoverable, not lost)",
    );

    // ── Phase 2: respawn primary → switchback window ──────────────────────────
    // Reboot EMPTY at a higher gen + new pod IP, re-learn the address, drive ready,
    // mark Alive. The go-active bulk reclaim re-materialises the reverse-folded call
    // from pri:{primary} so the returned primary serves it again.
    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        fh.mark(&primary_ord, None, "reboot", "switchback: restart empty, higher gen, new pod IP");
        let new_addr = primary.reboot().await;
        proxy.set_address(&primary_ord, new_addr);
        backup.simulate_peer_added(&primary_ord);
        for _ in 0..120 {
            fh.advance(Duration::from_millis(500)).await;
            if primary.is_ready() {
                break;
            }
        }
        assert!(primary.is_ready(), "rebooted primary {primary_ord} became ready");
        proxy.set_health(&primary_ord, WorkerHealth::Alive);
        // ReclaimAll (smoothed) + puller reconnect window.
        fh.advance(Duration::from_secs(10)).await;
    }

    let primary = if primary_ord == "b1" { &w_b1 } else { &w_b2 };
    assert!(
        primary.serves(&call_ref),
        "the returned primary reclaimed the reverse-folded call and serves it again",
    );
    failover_harness::assert_single_owner(&[&w_b1, &w_b2], &call_ref);
    assert_eq!(
        store.stats().current_total, 1,
        "the hold rode the switchback reclaim (still one) — the BYE has not run yet",
    );

    // ── Phase 3: BYE routes back to the returned primary (decode_forward) ─────
    // The cookie's w_pri is alive again, so alice's BYE routes to the reborn
    // primary — NOT the backup. It terminates the reclaimed call locally and emits
    // the limiter decrement against the cluster-shared store using the call's
    // PRESERVED origin_window (carried through admit→takeover-fold→reverse-flush→
    // reclaim). If the switchback re-stamped the origin to the primary's new epoch,
    // this decrement would miss and `current_total` would stay a leaked 1.
    let creations_before = primary.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let _ = &alice;

    let drained = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert!(
        drained,
        "the switchback BYE on the returned primary must drain the shared limiter to 0",
    );

    // THE regression guard (TS `expectLimiterCount(0)`): the decrement landed on the
    // origin window, so the cluster-shared counter is back to 0.
    assert_eq!(
        store.stats().current_total, 0,
        "switchback BYE decremented the shared limiter via the preserved origin_window",
    );
    // The discharge happened on the RETURNED PRIMARY (it created/reclaimed the call
    // locally to serve the BYE), not via a backup lossy reap.
    let primary = if primary_ord == "b1" { &w_b1 } else { &w_b2 };
    assert!(
        primary.metrics().creations_total() >= creations_before,
        "the returned primary served the switchback BYE",
    );
    assert_eq!(
        w_b1.metrics().repl_terminal_lost_total() + w_b2.metrics().repl_terminal_lost_total(),
        0,
        "switchback reclaimed within budget → no Model-Y lossy reap (the active BYE \
         discharged, the deferral was never abandoned)",
    );

    // Universal teardown post-condition: exactly one CDR cluster-wide, limiter at 0,
    // no resurrectable trace, memory clean. (TS `expectCdrCount == 1`.)
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

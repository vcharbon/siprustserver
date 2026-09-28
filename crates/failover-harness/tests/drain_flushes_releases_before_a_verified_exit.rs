//! **A withdrawn worker's caught-up exit comes after its release flush, at a
//! moment its backup is verified current (ADR-0031 D2, ADR-0038 decision 9).**
//!
//! The worker drains with a limiter release in flight: the limiter answers
//! only 1.5 s after the caught-up precondition first holds. The worker keeps
//! serving its live call meanwhile, and the call's limiter refresh logs
//! changes for the backup during the flush. The drain returns once the
//! release landed and the backup has applied everything logged, never at the
//! earlier caught-up moment.
//!
//! ```text
//!   established · counted on x, y, z · the survivor's Backup flow at the head
//!   one release queued on the elder, its limiter answer held back
//!   t0        withdrawn, drain begins
//!   t0+1 s    the floor: caught up, the flush begins (the release in flight)
//!   t0+2.5 s+ the limiter answers as the live call logs a change: the
//!             release lands, the flush ends with the backup behind
//!   then      exit caught_up once the backup applied that change
//!   then      the process exits, a replacement reclaims the call, BYE ends it
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, RefreshAnswer, RefreshCall, ReleaseAnswer,
};
use b2bua::limiter_http::HttpCallLimiter;
use call_limiter::wire::AdmitEntry;
use call_limiter::{AdmitResult, CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, DrainBounds, DrainExit,
    FailoverHarness, Partition, ReplicatedB2buaSut, WorkerHealth,
};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;
use tokio::sync::watch;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The limiters every call holds.
const IDS: [&str; 3] = ["x", "y", "z"];

/// The deployed shutdown bounds.
const FLOOR: Duration = Duration::from_millis(1000);
const BOUNDS: DrainBounds = DrainBounds {
    grace: Duration::from_secs(5),
    floor: FLOOR,
    release_flush: Duration::from_secs(3),
};
/// When the limiter may answer the queued release, from the drain's start.
const ANSWERED_AT: Duration = Duration::from_millis(2_500);
/// The step the timeline advances by, so an exit is seen within it.
const STEP: Duration = Duration::from_millis(10);

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// The limiter client, except that a release waits until `open` reads true.
struct HeldReleases {
    inner: Arc<dyn CallLimiter>,
    open: watch::Receiver<bool>,
}

#[async_trait]
impl CallLimiter for HeldReleases {
    async fn admit(&self, key: &str, entries: &[LimiterEntry], drop: bool) -> AdmitOutcome {
        self.inner.admit(key, entries, drop).await
    }
    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        let mut open = self.open.clone();
        let _ = open.wait_for(|open| *open).await;
        self.inner.release(keys).await
    }
    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        self.inner.refresh(calls).await
    }
    fn report_to(&self, reports: b2bua::limiter::LimiterReports) {
        self.inner.report_to(reports);
    }
}

fn decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.new_ruri = None;
                r.call_limiter =
                    IDS.iter().map(|id| CallLimiterEntry { id: (*id).into(), limit: 10 }).collect();
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

#[tokio::test(start_paused = true)]
async fn a_withdrawn_workers_exit_waits_for_its_release_flush_and_a_current_backup() {
    // A refresh every second: the live call logs changes during the flush.
    let mut fh =
        FailoverHarness::new("drain-flushes-releases-before-a-verified-exit", &["b1", "b2"])
            .with_worker_tune(|c| c.limiter_refresh_sec = 1);

    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _server: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();
    let (open, open_rx) = watch::channel(true);
    let client = |http: &SimulatedHttpNetwork| -> Arc<dyn CallLimiter> {
        let inner: Arc<dyn CallLimiter> = Arc::new(HttpCallLimiter::new(
            Arc::new(http.clone()),
            laddr(),
            Duration::from_millis(150),
        ));
        Arc::new(HeldReleases { inner, open: open_rx.clone() })
    };
    let held = |store: &CallStore| IDS.map(|id| store.held(id));

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
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
            decision(),
            client(&http),
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
            decision(),
            client(&http),
        )
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── an established, replicated call counted on three limiters ─────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(held(&store), [1, 1, 1], "the call holds its three limiters");

    let (elder, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
    let call_ref =
        survivor.scan_one_backed_up(&pri_ord).await.expect("the call replicated to the backup");

    // ── a call that ended on the elder: its release waits, unanswered ─────────
    let ended = "ended-on-the-elder";
    let entries: Vec<AdmitEntry> =
        IDS.iter().map(|id| AdmitEntry { id: (*id).into(), limit: 10 }).collect();
    assert_eq!(store.admit(ended, &entries, false), AdmitResult::Admitted);
    open.send_replace(false);
    let queue = elder.limiter_release_queue().expect("the elder runs");
    queue.push(ended);
    fh.advance(STEP).await;
    assert_eq!(held(&store), [2, 2, 2]);
    assert!(queue.sending(), "the release is in flight, its answer held back");

    // ── withdrawn, draining: the floor passes caught up, the flush begins ─────
    let t0 = fh.now_ms();
    let since = |fh: &FailoverHarness| Duration::from_millis((fh.now_ms() - t0) as u64);
    fh.withdraw_routing(&pri_ord);
    let mut drain = fh.begin_drain_pending(elder, BOUNDS);
    while since(&fh) < FLOOR {
        fh.advance(STEP).await;
    }
    let head_at_floor = elder.changelog_head().expect("replicated");
    while since(&fh) < ANSWERED_AT {
        let early = drain.poll();
        assert!(early.is_none(), "the flush holds the exit, got {early:?} at +{:?}", since(&fh));
        fh.advance(STEP).await;
    }
    // The limiter answers just as the live call logs a change the backup has
    // not applied yet: an exit at the flush's end would find its backup behind.
    let before = elder.changelog_head().expect("replicated");
    while elder.changelog_head().expect("replicated") == before {
        assert!(drain.poll().is_none());
        fh.advance(Duration::from_millis(1)).await;
    }
    assert!(!elder.flow_caught_up(&bak_ord, Partition::Bak), "the backup is behind the change");
    open.send_replace(true);
    let mut outcome = None;
    while outcome.is_none() && since(&fh) < BOUNDS.grace + BOUNDS.release_flush {
        fh.advance(Duration::from_millis(1)).await;
        outcome = drain.poll();
    }
    drop(drain);
    let outcome = outcome.expect("the drain returned");

    // ── the exit came after the flush, at a verified caught-up moment ─────────
    assert_eq!(outcome.exit, DrainExit::CaughtUp, "in {:?}", outcome.elapsed);
    assert!(outcome.elapsed >= ANSWERED_AT, "the exit waited for the flush: {:?}", outcome.elapsed);
    assert_eq!((outcome.release_flush.queued, outcome.release_flush.given_up), (1, 0));
    assert_eq!(held(&store), [1, 1, 1], "the ended call's release landed before the exit");
    let head = elder.changelog_head().expect("replicated");
    assert!(head > head_at_floor, "the live call logged changes during the flush");
    assert!(
        elder.flow_caught_up(&bak_ord, Partition::Bak),
        "the backup applied everything logged, the flush's changes included, before the exit"
    );
    assert_eq!(elder.metrics().drain_release_flushes("sent"), 1);

    // ── the process exits; a replacement reclaims the call and it ends ────────
    fh.depart(&pri_ord);
    elder.crash();
    fh.advance(Duration::from_millis(200)).await;
    let replacement = fh.spawn_replacement(&pri_ord).await;
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            replacement.is_ready() && replacement.serves(&call_ref)
        })
        .await;
    assert!(reclaimed, "the replacement reclaimed the handed-over call");
    fh.readmit(&pri_ord, replacement.sip_addr());
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(500)).await;

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let nodes: [&ReplicatedB2buaSut; 3] = [&*elder, survivor, &replacement];
    let drained = fh
        .settle_terminal(async || {
            let mut clean = true;
            for n in nodes {
                if !n.memory_clean() || n.holds_any_trace(&call_ref).await {
                    clean = false;
                }
            }
            clean
        })
        .await;
    assert!(drained, "the cluster drains the call within the settle budget");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&nodes[..], &call_ref), 1, "exactly one CDR across the cluster");
    assert_call_fully_released(&nodes[..], &call_ref).await;
    assert_eq!(held(&store), [0, 0, 0], "every limiter hold is released");

    drop((w_b1, w_b2, replacement, proxy));
}

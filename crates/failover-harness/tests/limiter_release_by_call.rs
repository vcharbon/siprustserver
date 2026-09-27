//! A call's limiter holds are released exactly once across a crash, whatever
//! copy of the call the releasing node holds.
//!
//! Two nodes can each release one call: the crashed primary before it died
//! and the backup after the takeover (a fold's flush lost with the crash), or
//! a rebooted primary's reclaim and the backup's reap of the same deferred
//! terminal. The release is keyed by the call, so the second one frees
//! nothing: a neighbour's hold on the same id is never taken.
//!
//! A counted call whose set lapsed while its primary was down (the lease is
//! shorter than a silent takeover window) is counted again by the first
//! refresh of the node that materialises it: the refresh re-registers the set.
//! An uncounted call's fold set lapses with its lease and is counted there.
//!
//! Each id carries one **witness** hold admitted outside the call, so a
//! surplus release reads below the witness instead of vanishing under the
//! store's floor at 0.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, CallReleaseResponse, NewCallResponse, ReleaseOutcome,
    ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use call::ReleaseEventKind;
use call_limiter::wire::AdmitEntry;
use call_limiter::{AdmitResult, CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use failover_harness::{
    assert_call_fully_over, assert_call_lost_no_cdr, cookie_field, FailoverHarness,
    ReplicatedB2buaSut, WorkerHealth, LEASE_OUTLIVING_THE_REPLICA_TTL, RULE_CSEQ_IN_DIALOG_ORDER,
};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
/// The media server a release reroute dials.
const MEDIA: &str = "127.0.0.1:5090";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// Every id a scenario may hold; each carries one witness hold.
const IDS: [&str; 3] = ["x", "y", "z"];

/// The replica TTL of a deferred terminal (`reboot_budget_sec`, the harness's
/// production parity value).
const REBOOT_BUDGET: Duration = Duration::from_secs(600);

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

fn ha_harness(name: &str) -> FailoverHarness {
    FailoverHarness::new(name, &["b1", "b2"])
}

/// ADR-0014's accepted keepalive-vs-backup-transaction overlap from this
/// instant on: while one leg has two potential owners, both can mint the same
/// `local_cseq + 1`; the accepted outcome is one call dropping cleanly, which
/// each case asserts through its limiter-drain checks.
fn accept_takeover_cseq_overlap(fh: &mut FailoverHarness) {
    fh.accept_rfc_deviations_from_now(
        RULE_CSEQ_IN_DIALOG_ORDER,
        "ADR-0014 accepted trade-off: dual-owner in-dialog CSeq overlap in the \
         takeover window — one call drops cleanly",
    );
}

fn limiter_client(http: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr(), Duration::from_millis(150)))
}

/// The shared limiter on its own simulated HTTP fabric (it survives crashes),
/// with one witness hold per id in [`IDS`].
struct LimiterRig {
    http: SimulatedHttpNetwork,
    store: Arc<CallStore>,
    _server: Box<dyn HttpServerHandle>,
}

impl LimiterRig {
    /// The rig under a lease outliving the replica TTL: the release a cell
    /// names is what frees the call.
    async fn serve() -> Self {
        Self::serve_with(LEASE_OUTLIVING_THE_REPLICA_TTL).await
    }

    async fn serve_with(cfg: LimiterConfig) -> Self {
        let http = SimulatedHttpNetwork::new();
        let store = Arc::new(CallStore::new(cfg, Clock::test_at(0)));
        for id in IDS {
            let witness = store.admit(
                &format!("witness-{id}"),
                &[AdmitEntry { id: id.into(), limit: 100 }],
                false,
            );
            assert_eq!(witness, AdmitResult::Admitted, "witness hold on {id}");
        }
        let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        let handle = http.serve(laddr(), server).await.unwrap();
        Self { http, store, _server: handle }
    }

    /// The call's holds on every id of [`IDS`]: the store's count less the
    /// witness. Negative = a release matched no hold of the call.
    fn holds(&self) -> [i64; 3] {
        IDS.map(|id| self.store.held(id) - 1)
    }

    /// Release the witnesses, so the universal post-conditions read the store
    /// empty.
    fn release_witnesses(&self) {
        for id in IDS {
            self.store.release(&format!("witness-{id}"));
        }
    }

    /// Extend every witness's lease.
    fn refresh_witnesses(&self) {
        for id in IDS {
            assert_eq!(
                self.store.refresh(&format!("witness-{id}"), &[id.to_string()]),
                call_limiter::RefreshResult::Extended,
                "witness on {id} is known"
            );
        }
    }
}

/// The cluster `call_ref` of the one call, read from the backup's replica
/// partition once the primary replicated it.
async fn replicated_call_ref(
    fh: &FailoverHarness,
    backup: &ReplicatedB2buaSut,
    primary_ord: &str,
) -> String {
    for _ in 0..50 {
        if let Some(rf) = backup.scan_one_backed_up(primary_ord).await {
            return rf;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    panic!("the backup holds the replicated call");
}

fn limiters(ids: &[&str]) -> Vec<CallLimiterEntry> {
    ids.iter().map(|id| CallLimiterEntry { id: (*id).into(), limit: 10 }).collect()
}

/// Two replicating workers behind the proxy, both on `decision` and the
/// shared limiter, both ready.
async fn spawn_workers(
    fh: &mut FailoverHarness,
    rig: &LimiterRig,
    decision: Arc<dyn CallDecisionEngine>,
) -> (failover_harness::ProxySut, ReplicatedB2buaSut, ReplicatedB2buaSut) {
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let w_b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &["b2"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            decision.clone(),
            limiter_client(&rig.http),
        )
        .await;
    let w_b2 = fh
        .spawn_worker_limited(
            "b2",
            "b2",
            B2,
            &["b1"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            decision,
            limiter_client(&rig.http),
        )
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready");
    (proxy, w_b1, w_b2)
}

/// The route toward bob holding `ids`, whose duration cap raises the
/// subscribed release consult; the reroute dials the media server with
/// `reroute`.
fn release_reroute_decision(
    ids: &'static [&'static str],
    reroute: &'static [&'static str],
) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.new_ruri = None;
                r.features.platform.max_duration_sec = 60;
                r.callback_context = Some("release-ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                r.call_limiter = limiters(ids);
                NewCallResponse::Route(r)
            })
            .on_release(move |_| {
                let mut r = route_to("127.0.0.1", 5090);
                r.new_ruri = Some(format!("sip:{MEDIA}"));
                r.call_limiter = limiters(reroute);
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    )
}

/// The primary crashes between a reroute fold and its replication flush. The
/// established call holds `[x, y]`, replicated to the backup; the replication
/// fabric is then cut and the duration cap reroutes the call to `[y, z]`: the
/// fold replaces the call's set on the limiter and dials the media server, and
/// its flush never lands. The primary crashes. The caller's BYE fails over to
/// the backup, whose copy still says `[x, y]`; its lossy cleanup releases the
/// call by `call_ref`, which frees what the limiter holds for it, `y` and `z`,
/// and nothing of `x`: the witness on `x` is intact and the call drains to 0.
#[tokio::test(start_paused = true)]
async fn a_crash_between_a_reroute_fold_and_its_flush_releases_the_call_once() {
    let mut fh = ha_harness("limiter-release-by-call-fold-flush-lost");
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let media = fh.agent("media", MEDIA).await;
    let rig = LimiterRig::serve().await;
    let (proxy, mut w_b1, mut w_b2) =
        spawn_workers(&mut fh, &rig, release_reroute_decision(&["x", "y"], &["y", "z"])).await;

    // ── establish A↔B on the primary; replicate ──────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let primary_ord = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [1, 1, 0], "the established call holds x and y");
    let backup_ord = if primary_ord == "b1" { "b2" } else { "b1" };
    let backup_sut = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref = replicated_call_ref(&fh, backup_sut, &primary_ord).await;

    // ── the fold's flush will never land: cut the replication fabric ─────
    fh.partition(&primary_ord, backup_ord);

    // ── the duration cap reroutes the call: [x, y] → [y, z] ──────────────
    fh.advance(Duration::from_secs(61)).await;
    assert!(
        media.try_receive_tolerating("INVITE", &[]).await.is_some(),
        "the reroute dials the media server",
    );
    assert_eq!(rig.holds(), [0, 1, 1], "the fold replaced the call's set with y and z");

    // ── the primary crashes with the fold's flush unlanded ───────────────
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    accept_takeover_cseq_overlap(&mut fh);
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // ── the BYE fails over to the backup, whose copy predates the fold ───
    let creations_before = backup.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let released = fh.settle_lossy_cleanup(async || rig.holds() == [0, 0, 0]).await;
    assert!(
        backup.metrics().creations_total() > creations_before,
        "backup processed the failed-over BYE",
    );
    assert!(
        released && rig.holds() == [0, 0, 0],
        "the takeover releases the call once, whatever set its copy names: the witness \
         on x is intact and the fold's y and z are drained; holds {:?}",
        rig.holds()
    );
    rig.release_witnesses();
    assert_call_lost_no_cdr(&[&w_b1, &w_b2], &call_ref, &rig.store).await;
}

/// The route toward bob holding `ids`.
fn limited_decision(ids: &'static [&'static str]) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.new_ruri = None;
                r.call_limiter = limiters(ids);
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// A rebooting primary's reclaim and the backup's reap both release one
/// deferred terminal. The call holds `[x, x, y]`; the primary crashes and the
/// caller's BYE fails over to the backup, which defers the terminal to the
/// crashed primary. The backup has been told the primary left the cluster, so
/// it never pulls the reborn one. The primary reboots inside the replica TTL,
/// reclaims the deferral and discharges it (one CDR, the call released); its
/// delete never reaches the backup, whose reap releases the same call once
/// the TTL lapses. The second release frees nothing: the witnesses are intact.
#[tokio::test(start_paused = true)]
async fn a_reboot_reclaim_and_the_backup_reap_release_the_call_once() {
    let mut fh = ha_harness("limiter-release-by-call-reclaim-and-reap");
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let rig = LimiterRig::serve().await;
    let (proxy, mut w_b1, mut w_b2) =
        spawn_workers(&mut fh, &rig, limited_decision(&["x", "x", "y"])).await;

    // ── establish on the primary; replicate ──────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let primary_ord = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [2, 1, 0], "the call holds x twice and y");
    let backup_sut = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref = replicated_call_ref(&fh, backup_sut, &primary_ord).await;

    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    // ── the primary crashes for the backup's whole view of it ────────────
    accept_takeover_cseq_overlap(&mut fh);
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_millis(300)).await;

    // ── the BYE fails over to the backup, which defers the terminal ──────
    let creations_before = backup.metrics().creations_total();
    let deferred_at = fh.now_ms();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;
    assert!(backup.metrics().creations_total() > creations_before, "backup served the BYE");
    assert_eq!(rig.holds(), [2, 1, 0], "the backup defers the release to the primary");

    // ── inside the TTL: the primary reboots and reclaims the deferral ────
    let reboot_at = deferred_at + REBOOT_BUDGET.as_millis() as i64 - 40_000;
    while fh.now_ms() < reboot_at {
        fh.advance(Duration::from_secs(10)).await;
    }
    assert_eq!(rig.holds(), [2, 1, 0], "still deferred right before the reboot");
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
    let discharged = fh
        .settle_terminal(async || primary.cdr_records().len() == 1 && rig.holds() == [0, 0, 0])
        .await;
    assert!(
        discharged,
        "the reclaim discharged the deferral: one CDR, the call released; holds {:?}",
        rig.holds()
    );
    assert!(
        fh.now_ms() < deferred_at + REBOOT_BUDGET.as_millis() as i64,
        "the reclaim ran inside the replica TTL",
    );

    // ── past the TTL: the backup's reap releases the same call again ─────
    let releases_before = rig.store.stats().releases_total;
    let reaped_at = deferred_at + REBOOT_BUDGET.as_millis() as i64 + 90_000;
    while fh.now_ms() < reaped_at {
        fh.advance(Duration::from_secs(30)).await;
    }
    assert_eq!(
        rig.store.stats().releases_total,
        releases_before + 1,
        "the backup's reap sent its release of the call",
    );
    assert_eq!(
        rig.holds(),
        [0, 0, 0],
        "the backup's reap released a call already released: the witnesses on x and y are intact",
    );
    rig.release_witnesses();
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &rig.store).await;
}

/// The primary crashes and the call is silent for longer than the lease: its
/// set lapses. The caller's re-INVITE then fails over to the backup, which
/// materialises the call and re-arms its timers; the past-due refresh fires at
/// once and re-registers the set, so the call is counted again within one
/// refresh of the takeover. The caller's BYE ends it on the backup (StayDead:
/// the deferred terminal is reaped) and the store drains to the witnesses.
#[tokio::test(start_paused = true)]
async fn a_call_whose_set_lapsed_while_its_primary_was_down_is_counted_again_on_takeover() {
    let mut fh = ha_harness("limiter-release-by-call-lapsed-then-takeover");
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let rig = LimiterRig::serve_with(LimiterConfig::default()).await;
    let (proxy, mut w_b1, mut w_b2) =
        spawn_workers(&mut fh, &rig, limited_decision(&["x", "y"])).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let primary_ord = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [1, 1, 0], "the established call holds x and y");
    let backup_sut = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref = replicated_call_ref(&fh, backup_sut, &primary_ord).await;

    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    // ── the primary crashes; the call is silent past the lease ───────────
    accept_takeover_cseq_overlap(&mut fh);
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    for _ in 0..15 {
        fh.advance(Duration::from_secs(10)).await;
        rig.refresh_witnesses();
    }
    rig.store.sweep_now();
    assert_eq!(rig.holds(), [0, 0, 0], "the set lapsed: nobody refreshed it");
    assert_eq!(rig.store.stats().lease_expired_calls, 1);

    // ── a re-INVITE fails over to the backup, whose refresh re-registers ──
    let mut reinv = dialog.request(InDialogMethod::Invite, None).await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await;
    dialog.ack(Some(ANSWER)).await;
    bob.receive("ACK").await;
    let counted_again = fh.settle_terminal(async || rig.holds() == [1, 1, 0]).await;
    assert!(
        counted_again,
        "the takeover's first refresh re-registered the set; holds {:?}",
        rig.holds()
    );
    assert_eq!(rig.store.stats().reregistered_calls, 1);
    assert_eq!(backup.metrics().limiter_refresh_reregistered_total(), 1);

    // ── the BYE ends the call on the backup; the reap frees it (no CDR) ──
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let released = fh
        .settle_lossy_cleanup(async || {
            rig.refresh_witnesses();
            rig.holds() == [0, 0, 0] && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(released, "the call drains to the witnesses; holds {:?}", rig.holds());
    rig.release_witnesses();
    assert_call_lost_no_cdr(&[&w_b1, &w_b2], &call_ref, &rig.store).await;
}

/// The reboot variant: the primary crashes, the call is silent past the
/// lease, and the primary reboots inside the replica TTL. Its reclaim
/// re-arms the call's timers; the past-due refresh re-registers the set, so
/// the call is counted again within one refresh of the reclaim. The caller's
/// BYE lands on the reborn primary, which releases the call once.
#[tokio::test(start_paused = true)]
async fn a_call_whose_set_lapsed_while_its_primary_was_down_is_counted_again_on_reclaim() {
    let mut fh = ha_harness("limiter-release-by-call-lapsed-then-reclaim");
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let rig = LimiterRig::serve_with(LimiterConfig::default()).await;
    let (proxy, mut w_b1, mut w_b2) =
        spawn_workers(&mut fh, &rig, limited_decision(&["x", "x", "y"])).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let primary_ord = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [2, 1, 0], "the call holds x twice and y");
    let backup_sut = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref = replicated_call_ref(&fh, backup_sut, &primary_ord).await;

    let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };

    // ── the primary crashes; the call is silent past the lease ───────────
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(&primary_ord);
    for _ in 0..15 {
        fh.advance(Duration::from_secs(10)).await;
        rig.refresh_witnesses();
    }
    rig.store.sweep_now();
    assert_eq!(rig.holds(), [0, 0, 0], "the set lapsed: nobody refreshed it");

    // ── the primary reboots and reclaims; its refresh re-registers ───────
    let new_addr = primary.reboot().await;
    proxy.set_address(&primary_ord, new_addr);
    fh.note_worker_rebound(&primary_ord, new_addr);
    backup.simulate_peer_added(&primary_ord);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        rig.refresh_witnesses();
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary re-hydrated from the backup");
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    let counted_again = fh
        .settle_terminal(async || {
            rig.refresh_witnesses();
            rig.holds() == [2, 1, 0]
        })
        .await;
    assert!(
        counted_again,
        "the reclaim's first refresh re-registered the set; holds {:?}",
        rig.holds()
    );
    assert_eq!(rig.store.stats().reregistered_calls, 1);

    // ── the BYE lands on the reborn primary, which releases once ─────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let drained = fh
        .settle_terminal(async || {
            rig.refresh_witnesses();
            rig.holds() == [0, 0, 0]
                && w_b1.cdr_records().len() + w_b2.cdr_records().len() == 1
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(drained, "the release drained the call; holds {:?}", rig.holds());
    rig.release_witnesses();
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &rig.store).await;
}

/// A fold admitted for an uncounted call, then a crash before the fold's
/// flush. The initial route states no limiter, so the call is uncounted; the
/// duration cap reroutes it to `[y, z]`, which the fold admits; the primary
/// crashes with the flush unlanded. The backup's copy says uncounted, so the
/// takeover never refreshes or releases: the set lapses with its lease and is
/// counted there (map Q7).
#[tokio::test(start_paused = true)]
async fn a_fold_set_of_an_uncounted_call_lost_with_the_primary_lapses_with_its_lease() {
    let mut fh = ha_harness("limiter-release-by-call-uncounted-fold-flush-lost");
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let media = fh.agent("media", MEDIA).await;
    let rig = LimiterRig::serve_with(LimiterConfig::default()).await;
    let (proxy, mut w_b1, mut w_b2) =
        spawn_workers(&mut fh, &rig, release_reroute_decision(&[], &["y", "z"])).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let primary_ord = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [0, 0, 0], "the uncounted call holds nothing");
    let backup_ord = if primary_ord == "b1" { "b2" } else { "b1" };
    let backup_sut = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref = replicated_call_ref(&fh, backup_sut, &primary_ord).await;

    // ── the fold's flush will never land: cut the replication fabric ─────
    fh.partition(&primary_ord, backup_ord);

    // ── the duration cap reroutes the uncounted call: the fold admits [y, z]
    for _ in 0..61 {
        fh.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    assert!(
        media.try_receive_tolerating("INVITE", &[]).await.is_some(),
        "the reroute dials the media server",
    );
    assert_eq!(rig.holds(), [0, 1, 1], "the fold's set is on the limiter");

    // ── the primary crashes with the fold's flush unlanded ───────────────
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    accept_takeover_cseq_overlap(&mut fh);
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // ── the BYE fails over to a copy that says uncounted ─────────────────
    let creations_before = backup.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let lapsed = fh
        .settle_lossy_cleanup(async || {
            rig.refresh_witnesses();
            rig.holds() == [0, 0, 0] && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(backup.metrics().creations_total() > creations_before, "backup served the BYE");
    assert!(lapsed, "the fold's set lapsed with its lease; holds {:?}", rig.holds());
    assert_eq!(rig.store.stats().lease_expired_calls, 1, "counted as a lapsed lease");
    assert_eq!(rig.store.stats().releases_total, 0, "nobody released the uncounted call");
    rig.release_witnesses();
    assert_call_lost_no_cdr(&[&w_b1, &w_b2], &call_ref, &rig.store).await;
}

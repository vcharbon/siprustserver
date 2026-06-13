//! The "call terminated on the backup" matrix (TODO `FixCallTerminateOnBackup`,
//! §6). Every cell drives one call to a terminal that is (in most cells) served
//! by the **backup**, then asserts the four universal invariants via
//! [`assert_call_fully_over`]: exactly one CDR across the cluster, the call fully
//! over everywhere (0 owners, no resurrectable replica, memory clean), and the
//! shared limiter released exactly once.
//!
//! These are written **RED first** (TDD): C1 (`bye_on_primary`) and C11
//! (`reinvite_on_backup`, the no-discharge guard rail) are the controls expected
//! to pass today; C2–C10 exercise the backup terminating a call and are expected
//! to break one of the invariants until the production fix (Model A vs B, decided
//! from these signatures) lands. Each cell's `FailoverHarness` name is unique, so
//! its unified callflow is written to `target/seq-reports/<cell>/report.html` on
//! drop (including on failure) for review.
//!
//! ## Misroute mechanism (primary stays ALIVE)
//! ADR-0014: a partition can route an in-dialog request to the backup at any time
//! with the primary perfectly healthy. We model that **without a production
//! routing change** by flipping the proxy's *health view* of the primary to
//! `Dead` (`set_health`) while leaving the primary worker running and owning the
//! live call. The proxy then cookie-routes the in-dialog request to the backup;
//! the primary never learns the call ended. (We use `spawn_proxy` — no health
//! probe — so `set_health` is authoritative and deterministic.)
// The cells share one uniform `Established { .. }` destructuring shape; whether a
// given binding needs `mut` varies per cell (only reboot cells mutate `fh`), so
// `unused_mut` is expected noise here.
#![allow(non_snake_case, unused_mut)]

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
    assert_call_fully_over, assert_call_lost_no_cdr, FailoverHarness, ProxySut, ReplicatedB2buaSut,
    WorkerHealth,
};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::{Agent, Dialog};
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::get_header;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

fn pri_from_cookie(rr: &str) -> String {
    let params = sip_message::message_helpers::parse_uri_params(rr);
    params.get("w_pri").cloned().unwrap_or_default()
}

fn bak_from_cookie(rr: &str) -> String {
    let params = sip_message::message_helpers::parse_uri_params(rr);
    params.get("w_bak").cloned().unwrap_or_default()
}

fn limiter_client(http: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(
        Arc::new(http.clone()),
        laddr(),
        Duration::from_millis(150),
    ))
}

/// Route every call to bob with a hold on trunk-A (cap 8 — comfortably above the
/// one call per cell) and a `max_duration_sec` GlobalDuration cap, so every cell
/// exercises a genuine limiter hold that must drain to zero. `max_duration_sec`
/// rides the route's platform features (`apply_route::arm_global_duration`); a
/// short value (C9) lets GlobalDuration fire long before the first 300 s
/// keepalive, the default (3600) keeps it out of the way for every other cell.
fn limited_decision_with_max_duration(max_duration_sec: i64) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![CallLimiterEntry {
                    id: "trunk-A".into(),
                    limit: 8,
                }];
                r.features.platform.max_duration_sec = max_duration_sec;
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

fn limited_decision() -> Arc<dyn CallDecisionEngine> {
    limited_decision_with_max_duration(3600)
}

/// Everything a cell needs after the call is established to the `Established`
/// safe-point (post-ACK, replicated primary → backup). No `Drop` impl, so a cell
/// can destructure it to independent owned locals (the `(primary, backup)`
/// split-borrow needs `w_b1`/`w_b2` as separate bindings).
struct Established {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    store: Arc<WindowStore>,
    /// The limiter HTTP server handle — must outlive the cell (drop closes it).
    lh: Box<dyn HttpServerHandle>,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    primary_ord: String,
    bak_ord: String,
    call_ref: String,
    /// alice's confirmed dialog (caller-initiated in-dialog requests).
    dialog: Dialog,
    /// bob's confirmed dialog (callee-initiated in-dialog requests).
    bob_dialog: Dialog,
}

/// Establish one limited call through the proxy to the `Established` state and
/// replicate it primary → backup. Leaves both nodes alive, no fault injected; the
/// cell injects its own. Discovers the cluster `call_ref` from the backup's
/// replica partition so the per-call CDR / trace assertions can key on it.
async fn establish(name: &str) -> Established {
    establish_with(name, limited_decision()).await
}

/// Like [`establish`] but with a caller-supplied decision engine (shared by both
/// workers) — e.g. a short-`max_duration` route for C9.
async fn establish_with(name: &str, decision: Arc<dyn CallDecisionEngine>) -> Established {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    // Shared limiter server on its own simulated HTTP fabric (survives crashes).
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    // No health probe: `set_health` is authoritative (deterministic misroute).
    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            decision.clone(), limiter_client(&http),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            decision.clone(), limiter_client(&http),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "[{name}] workers ready at steady state");

    // INVITE → 200 → ACK (Established).
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the proxy cookie")
        .to_string();
    let primary_ord = pri_from_cookie(&rr);
    let bak_ord = bak_from_cookie(&rr);
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let bob_dialog = uas.dialog();
    let dialog = call.ack().await;
    bob.receive("ACK").await;

    // Replicate the confirmed call primary → backup.
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(store.stats().current_total, 1, "[{name}] call admitted: one limiter hold");

    let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let mut call_ref = String::new();
    for _ in 0..50 {
        if let Some(rf) = backup.scan_one_backed_up(&primary_ord).await {
            call_ref = rf;
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    assert!(!call_ref.is_empty(), "[{name}] backup holds the replicated call ref");

    Established {
        fh, alice, bob, proxy, store, lh, w_b1, w_b2, primary_ord, bak_ord, call_ref, dialog, bob_dialog,
    }
}

/// Reboot the (crashed) primary EMPTY at a higher gen + new pod IP, re-learn its
/// address, drive it ready, mark it Alive, and let the go-active reclaim run. The
/// no-probe analogue of `runner::reboot_and_reclaim` (we set health directly).
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    backup: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &ProxySut,
) {
    fh.mark(primary_ord, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = primary.reboot().await;
    proxy.set_address(primary_ord, new_addr);
    backup.simulate_peer_added(primary_ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {primary_ord} became ready");
    proxy.set_health(primary_ord, WorkerHealth::Alive);
    // ReclaimAll (smoothed) + puller reconnect window.
    fh.advance(Duration::from_secs(10)).await;
}

/// Crash the primary and present the complete k8s death signal (dead in the proxy
/// + endpoint removed from the survivor's membership).
fn crash_primary(
    primary: &mut ReplicatedB2buaSut,
    backup: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &ProxySut,
) {
    primary.crash();
    proxy.set_health(primary_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(primary_ord);
}

// =============================================================================
// C1 — control: BYE handled by the primary, no fault. Expected GREEN today.
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c1_bye_on_primary__no_fault() {
    let Established {
        mut fh, alice, bob, proxy: _proxy, store, lh: _lh, w_b1, w_b2, primary_ord: _p, bak_ord: _b,
        call_ref, mut dialog, bob_dialog: _bd,
    } = establish("cterm-c1-bye-on-primary").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
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
    assert!(drained, "C1 must drain cleanly (control)");
    // Keep alice/bob reading their sockets for a few seconds past the terminal so
    // any SIP still in flight at teardown (a relayed final response, a redundant
    // in-dialog BYE the owner 481s, a retransmit toward a silent peer) is delivered
    // and consumed — never dropped into an about-to-close endpoint ("lost in
    // transit") or left unread (a queueLeak at bind close).
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

// =============================================================================
// C2 — BYE on the backup, primary ALIVE (misroute). The backup self-releases
// (no CDR / no limiter release), the live primary never learns the call ended.
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c2_bye_on_backup__primary_alive__misroute() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, w_b1, w_b2, primary_ord, bak_ord: _b,
        call_ref, mut dialog, bob_dialog: _bd,
    } = establish("cterm-c2-bye-backup-primary-alive").await;

    // Misroute: primary looks Dead to the proxy but keeps running + owning.
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = &alice;

    let _ = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    // Keep alice/bob reading their sockets for a few seconds past the terminal so
    // any SIP still in flight at teardown (a relayed final response, a redundant
    // in-dialog BYE the owner 481s, a retransmit toward a silent peer) is delivered
    // and consumed — never dropped into an about-to-close endpoint ("lost in
    // transit") or left unread (a queueLeak at bind close).
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

// =============================================================================
// C3 — BYE on the backup, primary ALIVE, PEER SILENT. bob never 200s the relayed
// BYE: the served txn clears only at Timer F (~32 s) and CallQuiesced preempts.
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c3_bye_on_backup__primary_alive__peer_silent() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, w_b1, w_b2, primary_ord, bak_ord: _b,
        call_ref, mut dialog, bob_dialog: _bd,
    } = establish("cterm-c3-bye-backup-peer-silent").await;

    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;

    // alice BYEs → relayed to the backup → relayed to bob, who STAYS SILENT.
    let _bye = dialog.bye().await;
    let _buas = bob.receive("BYE").await; // confirm relay; do NOT respond.
    let _ = &alice;

    let _ = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    // Keep alice/bob reading their sockets for a few seconds past the terminal so
    // any SIP still in flight at teardown (a relayed final response, a redundant
    // in-dialog BYE the owner 481s, a retransmit toward a silent peer) is delivered
    // and consumed — never dropped into an about-to-close endpoint ("lost in
    // transit") or left unread (a queueLeak at bind close).
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

// =============================================================================
// C4 — BYE on the backup, primary CRASHED, never returns (StayDead).
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c4_bye_on_backup__primary_crashed__stay_dead() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, mut w_b1, mut w_b2, primary_ord, bak_ord: _b,
        call_ref, mut dialog, bob_dialog: _bd,
    } = establish("cterm-c4-bye-backup-crashed-staydead").await;

    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        crash_primary(primary, backup, &primary_ord, &proxy);
    }
    fh.advance(Duration::from_millis(300)).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = &alice;

    // StayDead (Model Y, ADR-0020 X3): the backup deferred a `Terminated` body and
    // the primary CRASHED for good — it never reclaims. The primary is the sole CDR
    // authority, so the CDR is LOST (the accepted double-failure). But the backup's
    // periodic reap MUST still release the limiter hold + free the replica memory
    // once the deferral's replica TTL (`reboot_budget`) expires. Advance past it so
    // the reap runs, then assert the StayDead contract: NO CDR, limiter back to 0,
    // no trace left.
    let _ = fh
        .settle_lossy_cleanup(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_lost_no_cdr(&[&w_b1, &w_b2], &call_ref, &store).await;
    assert_eq!(
        w_b1.metrics().repl_terminal_lost_total() + w_b2.metrics().repl_terminal_lost_total(),
        1,
        "exactly one lost-CDR cleanup counted across the cluster",
    );
}

// =============================================================================
// C5 — BYE on the backup, primary CRASHED, PEER SILENT, StayDead.
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c5_bye_on_backup__primary_crashed__peer_silent__stay_dead() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, mut w_b1, mut w_b2, primary_ord, bak_ord: _b,
        call_ref, mut dialog, bob_dialog: _bd,
    } = establish("cterm-c5-bye-backup-crashed-silent-staydead").await;

    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        crash_primary(primary, backup, &primary_ord, &proxy);
    }
    fh.advance(Duration::from_millis(300)).await;

    let _bye = dialog.bye().await;
    let _buas = bob.receive("BYE").await; // silent.
    let _ = &alice;

    // StayDead + PEER SILENT: the backup deferred a `Terminating` body (b-leg BYE
    // sent, no 200 from the silent peer) and the primary CRASHED for good. Same
    // contract as C4 — CDR lost, but the backup's reap still releases the limiter +
    // frees memory once the deferral TTL (`reboot_budget`) expires. This exercises
    // the **Terminating** shape of the lossy cleanup.
    let _ = fh
        .settle_lossy_cleanup(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_lost_no_cdr(&[&w_b1, &w_b2], &call_ref, &store).await;
    assert_eq!(
        w_b1.metrics().repl_terminal_lost_total() + w_b2.metrics().repl_terminal_lost_total(),
        1,
        "exactly one lost-CDR cleanup counted across the cluster",
    );
}

// =============================================================================
// C6 — BYE on the backup, primary CRASHED then REBOOTS + reclaims.
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c6_bye_on_backup__primary_crashed__reboot_reclaim() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, mut w_b1, mut w_b2, primary_ord, bak_ord: _b,
        call_ref, mut dialog, bob_dialog: _bd,
    } = establish("cterm-c6-bye-backup-crashed-reboot").await;

    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        crash_primary(primary, backup, &primary_ord, &proxy);
    }
    fh.advance(Duration::from_millis(300)).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = &alice;

    // Dead for 60 s — a real reboot gap, but well inside `reboot_budget` (600 s) so
    // the backup's deferred `Terminated` is still held (NOT yet lossy-cleaned). The
    // rebooting primary must reclaim it and discharge: ONE CDR, limiter back to 0.
    fh.advance(Duration::from_secs(60)).await;

    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;
    }

    let _ = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
    // The primary discharged on reclaim — the backup must NOT have lossy-cleaned it
    // (the deferral was reclaimed well before its TTL), so no CDR was lost.
    assert_eq!(
        w_b1.metrics().repl_terminal_lost_total() + w_b2.metrics().repl_terminal_lost_total(),
        0,
        "reboot-within-budget reclaimed the deferral; no lossy cleanup should fire",
    );
}

// =============================================================================
// C7 — THE ENDURANCE BUG: BYE on the backup, peer silent, primary CRASHED then
// reboots + reclaims a zombie that pins the limiter until keepalive-timeout.
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c7_bye_on_backup__primary_crashed__peer_silent__reboot() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, mut w_b1, mut w_b2, primary_ord, bak_ord: _b,
        call_ref, mut dialog, bob_dialog: _bd,
    } = establish("cterm-c7-bye-backup-silent-reboot").await;

    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        crash_primary(primary, backup, &primary_ord, &proxy);
    }
    fh.advance(Duration::from_millis(300)).await;

    let _bye = dialog.bye().await;
    let _buas = bob.receive("BYE").await; // silent.
    let _ = &alice;

    // Dead for 60 s (inside `reboot_budget`): the backup holds a deferred
    // **Terminating** body (b-leg BYE sent, no 200 from the silent peer). The
    // rebooting primary must reclaim it, FORCE it terminal, and discharge: ONE CDR,
    // limiter back to 0 — the Terminating shape of the reboot-reclaim discharge.
    fh.advance(Duration::from_secs(60)).await;

    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;
    }

    let _ = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
    // Reclaimed within budget → no lossy cleanup, no lost CDR.
    assert_eq!(
        w_b1.metrics().repl_terminal_lost_total() + w_b2.metrics().repl_terminal_lost_total(),
        0,
        "reboot-within-budget reclaimed the deferral; no lossy cleanup should fire",
    );
}

// =============================================================================
// C10 — SPLIT-BRAIN: both nodes serve a terminal for the same dialog. alice BYEs
// to the backup (misroute), then bob BYEs to the (re-Alive) primary. The
// exactly-once stress: still EXACTLY ONE CDR, one limiter release.
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c10_bye_split_brain__primary_and_backup() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, w_b1, w_b2, primary_ord, bak_ord: _b,
        call_ref, mut dialog, mut bob_dialog,
    } = establish("cterm-c10-split-brain").await;

    // (1) Misroute alice's BYE to the backup; the live primary still owns it.
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;
    let mut bye_a = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye_a.expect(200).await;
    fh.advance(Duration::from_millis(500)).await;

    // (2) Heal the misroute; bob now BYEs the SAME dialog. Under Model Y, alice's
    //     BYE via the backup already ENDED the call — the live primary reconciled the
    //     backup's deferred terminal and discharged it exactly once — so bob's
    //     redundant BYE finds NO call at the primary → 481 (RFC-correct; the call is
    //     gone). alice therefore never receives a relayed BYE. The split-brain
    //     invariant under test is EXACTLY-ONE CDR cluster-wide despite both parties
    //     issuing a terminal, which `assert_call_fully_over` checks below.
    //
    //     bob MUST receive that 481: the LB proxy relays the owning b2bua's 481 back
    //     down bob's BYE client transaction (the relayed response carries bob's own
    //     top Via branch, so it matches). This only fails to land if the test tears
    //     down before pumping the clock — `expect_tolerating` auto-advances under the
    //     paused runtime, completing the BYE → 481 → bob round trip.
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(200)).await;
    let mut bye_b = bob_dialog.bye().await; // redundant terminal → 481 at the primary
    bye_b.expect_tolerating(481, &["OPTIONS"]).await;
    let _ = &alice;

    let _ = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    // Keep alice/bob reading their sockets for a few seconds past the terminal so
    // any SIP still in flight at teardown (a relayed final response, a redundant
    // in-dialog BYE the owner 481s, a retransmit toward a silent peer) is delivered
    // and consumed — never dropped into an about-to-close endpoint ("lost in
    // transit") or left unread (a queueLeak at bind close).
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

// =============================================================================
// C11 — GUARD RAIL: a NON-terminal in-dialog request (re-INVITE) served by the
// backup must NOT discharge — the call genuinely continues at the primary. We
// then end the call normally and assert exactly one CDR. Expected GREEN today and
// after the fix (any fix that discharges on the re-INVITE is wrong).
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c11_reinvite_on_backup__primary_alive__no_terminal() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, w_b1, w_b2, primary_ord, bak_ord: _b,
        call_ref, mut dialog, bob_dialog: _bd,
    } = establish("cterm-c11-reinvite-backup-no-terminal").await;

    // Misroute a non-terminal re-INVITE to the backup.
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;
    let mut tx = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut peer = bob.receive("INVITE").await;
    peer.respond(200, "OK").with_sdp(ANSWER).await;
    tx.expect(200).await;
    dialog.ack(Some(ANSWER)).await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    // The call must STILL be alive (no discharge on the non-terminal request).
    assert!(
        w_b1.serves(&call_ref) || w_b2.serves(&call_ref),
        "C11 guard rail: a non-terminal re-INVITE must NOT end the call",
    );
    assert_eq!(store.stats().current_total, 1, "C11: limiter hold still held mid-call");

    // Now end it normally (heal the misroute first so the BYE reaches the owner).
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(200)).await;
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
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
    assert!(drained, "C11 must drain cleanly after a normal hangup");
    // Keep alice/bob reading their sockets for a few seconds past the terminal so
    // any SIP still in flight at teardown (a relayed final response, a redundant
    // in-dialog BYE the owner 481s, a retransmit toward a silent peer) is delivered
    // and consumed — never dropped into an about-to-close endpoint ("lost in
    // transit") or left unread (a queueLeak at bind close).
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

// =============================================================================
// C8 — terminal driven by KEEPALIVE-TIMEOUT, NO inbound BYE. The primary crashes,
// reboots, and reclaims the still-`Active` call; the reclaimed copy arms its
// keepalive, probes both legs, NEITHER leg answers (the peer is gone), and the
// keepalive-timeout tears the call down — the terminal fires on the reclaimed
// (takeover) copy with no BYE ever sent. (Under reactive-only takeover + ADR-0014
// self-release, the survivor never durably owns an idle `Active` call, so the
// only node that can drive this terminal post-crash is the reboot-reclaimed
// primary — which is exactly the resurrection the matrix must pin.)
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c8_keepalive_timeout_on_backup__reboot() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, mut w_b1, mut w_b2, primary_ord, bak_ord: _b,
        call_ref, dialog: _d, bob_dialog: _bd,
    } = establish("cterm-c8-keepalive-timeout-reboot").await;
    let _ = (&alice, &bob);

    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        crash_primary(primary, backup, &primary_ord, &proxy);
    }
    fh.advance(Duration::from_millis(300)).await;
    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;
    }

    // NO BYE. NO keepalive answers. The settle pump crosses keepalive_interval
    // (300 s) + keepalive_timeout (45 s); the reclaimed copy's unanswered probe
    // must tear the call down and discharge it exactly once.
    let _ = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    // Keep alice/bob reading their sockets for a few seconds past the terminal so
    // any SIP still in flight at teardown (a relayed final response, a redundant
    // in-dialog BYE the owner 481s, a retransmit toward a silent peer) is delivered
    // and consumed — never dropped into an about-to-close endpoint ("lost in
    // transit") or left unread (a queueLeak at bind close).
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

// =============================================================================
// C9 — terminal driven by MAX-DURATION (GlobalDuration), NO inbound BYE. Same
// shape as C8 but the reclaimed call hits its absolute 120 s GlobalDuration cap
// (fires before the 300 s keepalive). This is the 1780-like resurrection: the
// reclaimed copy's GlobalDuration must discharge it cleanly, not leave a zombie
// holding its limiter slot until the (real, 1 h) cap.
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c9_max_duration_on_backup__reboot() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, mut w_b1, mut w_b2, primary_ord, bak_ord: _b,
        call_ref, dialog: _d, bob_dialog: _bd,
    } = establish_with(
        "cterm-c9-max-duration-reboot",
        limited_decision_with_max_duration(120),
    )
    .await;
    let _ = (&alice, &bob);

    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        crash_primary(primary, backup, &primary_ord, &proxy);
    }
    fh.advance(Duration::from_millis(300)).await;
    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;
    }

    let _ = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    // Keep alice/bob reading their sockets for a few seconds past the terminal so
    // any SIP still in flight at teardown (a relayed final response, a redundant
    // in-dialog BYE the owner 481s, a retransmit toward a silent peer) is delivered
    // and consumed — never dropped into an about-to-close endpoint ("lost in
    // transit") or left unread (a queueLeak at bind close).
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

// =============================================================================
// C12 — BYE processed by the PRIMARY, then the primary reboots: does it emit an
// in-dialog OPTIONS keepalive for that (already-terminated) call? (The case you
// asked about — it was NOT covered by C1–C11, all of which terminate on the
// backup.) The terminal delete (`RemoveCall`) propagates to the backup over the
// LIVE replication stream before the crash, so the reboot reclaims NOTHING for
// this call → no resurrection → no spurious in-dialog OPTIONS. Asserts exactly
// that (fully over, one CDR, limiter released once).
//
// NOTE ON THE BUGGY VARIANT: the endurance-shaped failure (the delete is killed
// *before* it replicates → backup keeps a stale `Active` body → reboot reclaims
// a zombie that keepalives a dead call) could not be constructed with the current
// fabric primitives: `partition`/`Cut` are keyed on the workers' *listen*
// addresses, but the live puller stream uses an ephemeral local addr, so those
// faults block only *new* connects — the in-flight delete still drains over the
// existing stream. Reproducing the buggy variant needs a new harness primitive
// that drops in-flight frames on the source's crash (or a `Cut` keyed on the live
// stream, not the listen addr). Tracked as a follow-up; this cell pins the benign
// (and currently correct) behaviour as a regression guard.
// =============================================================================
#[tokio::test(start_paused = true)]
async fn c12_bye_on_primary__then_reboot__no_resurrection() {
    let Established {
        mut fh, alice, bob, proxy, store, lh: _lh, mut w_b1, mut w_b2, primary_ord, bak_ord: _b,
        call_ref, mut dialog, bob_dialog: _bd,
    } = establish("cterm-c12-bye-primary-then-reboot").await;

    // alice BYEs → routed to the (healthy) primary → bob 200 → alice 200. The
    // primary discharges (CDR + limiter release + RemoveCall) and replicates the
    // delete to the backup over the live stream.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = &alice;
    fh.advance(Duration::from_millis(500)).await;

    // NON-VACUITY GUARD: the delete reached the backup, so the reboot has nothing
    // stale to reclaim. (If this ever flips to "still holds", the buggy-variant
    // resurrection is now reachable here and the rest of the cell must catch it.)
    {
        let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
        assert!(
            !backup.holds_any_trace(&call_ref).await,
            "C12 precondition: the primary-processed BYE's delete must replicate to \
             the backup before reboot (else a stale Active replica is reclaimable)",
        );
    }

    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        crash_primary(primary, backup, &primary_ord, &proxy);
    }
    fh.advance(Duration::from_millis(300)).await;
    {
        let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;
    }

    // The rebooted primary must reclaim nothing for this call and emit no in-dialog
    // OPTIONS for it. Settle past keepalive_interval + timeout and assert.
    let _ = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    // Keep alice/bob reading their sockets for a few seconds past the terminal so
    // any SIP still in flight at teardown (a relayed final response, a redundant
    // in-dialog BYE the owner 481s, a retransmit toward a silent peer) is delivered
    // and consumed — never dropped into an about-to-close endpoint ("lost in
    // transit") or left unread (a queueLeak at bind close).
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

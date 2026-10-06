//! A call that fails open stays in its node's uncounted-calls gauge on
//! whichever node holds it resident.
//!
//! The call's `fail_open` mark is replicated with its limiter state, so the
//! backup that takes the call over and the rebooted primary that reclaims it
//! each count it while it is resident on them, and each stops counting it
//! when it leaves them (the takeover copy's self-release, the call's end).

use call::LimiterEntry;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::http::HttpCallLimiter;
use b2bua::limiter::CallLimiter;
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use failover_harness::{
    assert_call_fully_over, cookie_field, FailoverHarness, ReplicatedB2buaSut, WorkerHealth,
    RULE_CSEQ_IN_DIALOG_ORDER,
};
use http_net::{Fault, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5081";
const B2: &str = "127.0.0.1:5082";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// The route toward bob holding `[x, y]`.
fn limited_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.new_ruri = None;
                r.call_limiter = ["x", "y"]
                    .iter()
                    .map(|id| LimiterEntry { id: (*id).into(), limit: 10 })
                    .collect();
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// One worker on the shared limiter.
async fn spawn_worker(
    fh: &mut FailoverHarness,
    http: &SimulatedHttpNetwork,
    ordinal: &'static str,
    addr: &'static str,
    peer: &'static str,
) -> ReplicatedB2buaSut {
    let limiter: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr(), Duration::from_millis(150)));
    fh.spawn_worker_limited(
        ordinal,
        ordinal,
        addr,
        &[peer],
        ("127.0.0.1", 5070),
        ("127.0.0.1", 5080),
        limited_decision(),
        limiter,
    )
    .await
}

/// The call's initial admit never reaches the limiter (the limiter is cut):
/// the call runs uncounted on its primary. The primary crashes; the caller's
/// re-INVITE fails over to the backup, whose takeover copy counts in the
/// backup's gauge while resident and leaves it once shed. The primary reboots
/// and reclaims the call, which counts in its gauge again, until the caller's
/// BYE ends the call there: every gauge reads 0, the call released its key.
#[tokio::test(start_paused = true)]
async fn a_call_failing_open_counts_on_the_node_it_is_resident_on() {
    let mut fh =
        FailoverHarness::new("limiter-uncounted-across-takeover-and-reclaim", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _server = http.serve(laddr(), server).await.unwrap();
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 = spawn_worker(&mut fh, &http, "b1", B1, "b2").await;
    let mut w_b2 = spawn_worker(&mut fh, &http, "b2", B2, "b1").await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready");

    // ── the initial admit fails: the call runs uncounted ─────────────────
    http.apply_fault(Fault::Cut { dst: laddr() });
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    http.apply_fault(Fault::Resume { dst: laddr() });
    let primary_ord = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    assert_eq!(primary.metrics().limiter().uncounted_calls(), 1, "resident on the primary");
    assert_eq!(backup.metrics().limiter().uncounted_calls(), 0, "a replica is not resident");
    assert_eq!(store.stats().current_total, 0, "the admit never landed");
    let mut call_ref = None;
    for _ in 0..50 {
        call_ref = backup.scan_one_backed_up(&primary_ord).await;
        if call_ref.is_some() {
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    let call_ref = call_ref.expect("the backup holds the replicated call");

    // ── the primary crashes; the re-INVITE fails over to the backup ──────
    fh.accept_rfc_deviations_from_now(
        RULE_CSEQ_IN_DIALOG_ORDER,
        "ADR-0014 accepted trade-off: dual-owner in-dialog CSeq overlap in the \
         takeover window — one call drops cleanly",
    );
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_secs(5)).await;
    let mut reinv = dialog.request(InDialogMethod::Invite, None).await;
    let mut bob_uas = bob.receive("INVITE").await;
    assert_eq!(
        backup.metrics().limiter().uncounted_calls(),
        1,
        "the takeover copy fails open as its primary's did"
    );
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await;
    dialog.ack(Some(ANSWER)).await;
    bob.receive("ACK").await;
    let shed = fh.settle_terminal(async || backup.metrics().limiter().uncounted_calls() == 0).await;
    assert!(shed, "the takeover copy's self-release leaves the backup's gauge");

    // ── the primary reboots and reclaims the call ────────────────────────
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
    let reclaimed =
        fh.settle_terminal(async || primary.metrics().limiter().uncounted_calls() == 1).await;
    assert!(reclaimed, "the reclaimed call fails open on the primary again");
    assert_eq!(backup.metrics().limiter().uncounted_calls(), 0);

    // ── the BYE ends the call on the primary ─────────────────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let ended = fh
        .settle_terminal(async || {
            primary.cdr_records().len() == 1
                && store.stats().releases_total == 1
                && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(ended, "one CDR and the call's release, owed by its admit request");
    assert_eq!(primary.metrics().limiter().uncounted_calls(), 0, "the call's end");
    assert_eq!(backup.metrics().limiter().uncounted_calls(), 0);
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
}

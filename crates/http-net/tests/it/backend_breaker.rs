//! The backend breaker under traffic shapes the window rule must survive: a
//! hung backend under a burst, a cap whose waiters never send, a probe that
//! panics; and the probe's reach while the name service fails or the backend
//! moves.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use http_net::backend::metrics;
use http_net::{
    AttemptError, BreakerConfig, ClientSettings, HttpRequest, HttpTransport, LOOKUP_BOUND,
};
use http_net_testkit::{Behaviour, Fault, SimulatedHttpNetwork};
use sip_clock::testkit::{advance_settled, settle};

use crate::backend::{Backend, Rig, ATTEMPT};

/// Three requests sent at once to a hung backend, nothing else: each attempt
/// times out at 2.5 s, past the 2 s window, and the breaker opens all the
/// same: the window saw three requests in flight and no response.
#[tokio::test(start_paused = true)]
async fn a_hung_backend_under_a_burst_opens_the_breaker() {
    let rig = Arc::new(Rig::new("hung-burst:8080", ClientSettings::default()).await);
    rig.behave(Behaviour::Silent);
    let burst: Vec<_> = (0..3)
        .map(|_| {
            let rig = rig.clone();
            tokio::spawn(async move { rig.send().await })
        })
        .collect();
    for request in burst {
        let _ = request.await.unwrap();
    }
    assert!(rig.client.is_open(), "three requests, no response for 2.5 s");
    assert_eq!(rig.transport.sent(), 3, "the opening timeouts are not retried");
}

/// Cap 1, the backend blackholed (its connects never complete within any
/// budget): one request holds the slot for its 2.5 s, twenty more arrive one
/// every 100 ms with a 300 ms budget, wait for the slot and give up unsent.
/// Only the one request sent reaches the breaker, so it stays closed.
#[tokio::test(start_paused = true)]
async fn requests_that_never_got_a_slot_never_reach_the_breaker() {
    let net = SimulatedHttpNetwork::new().with_connect_timeout(Duration::from_secs(60));
    let breaker = BreakerConfig::probing("/health");
    let rig = Arc::new(
        Rig::on("slot-wait:8080", ClientSettings::with_max_in_flight(1), breaker, net).await,
    );
    rig.behave(Behaviour::Blackhole);
    let holder = tokio::spawn({
        let rig = rig.clone();
        async move { rig.client.attempt(HttpRequest::post("/call", Vec::new()), ATTEMPT).await }
    });
    let mut waiters = Vec::new();
    for _ in 0..20 {
        advance_settled(Duration::from_millis(100)).await;
        let waiter = rig.clone();
        waiters.push(tokio::spawn(async move {
            waiter
                .client
                .attempt(HttpRequest::post("/call", Vec::new()), Duration::from_millis(300))
                .await
        }));
        assert!(!rig.client.is_open(), "a waiter that gave up is no request sent");
    }
    for waiter in waiters {
        assert_eq!(waiter.await.unwrap(), Err(AttemptError::Saturated));
    }
    assert_eq!(holder.await.unwrap(), Err(AttemptError::Timeout));
    assert!(!rig.client.is_open(), "one request sent and unanswered");
    assert_eq!(rig.transport.sent(), 1);
    assert_eq!(metrics::breaker_transitions("slot-wait:8080", true), 0);
    assert_eq!(metrics::slot_expired("slot-wait:8080"), 20);
}

/// A probe that panics is run again the next period, the breaker staying
/// open meanwhile; once a probe completes on an answering backend, it closes.
#[tokio::test(start_paused = true)]
async fn a_probe_that_panics_runs_again_each_period() {
    let rig = Rig::new("probe-panic:8080", ClientSettings::default()).await;
    let panicking = Arc::new(AtomicBool::new(true));
    let probes = Arc::new(AtomicUsize::new(0));
    rig.client.report_probes_to(Arc::new({
        let panicking = panicking.clone();
        let probes = probes.clone();
        move |_| {
            probes.fetch_add(1, Ordering::SeqCst);
            assert!(!panicking.load(Ordering::SeqCst), "a reporting probe that panics");
        }
    }));
    rig.client.trip();
    advance_settled(Duration::from_millis(3_100)).await;
    assert_eq!(probes.load(Ordering::SeqCst), 3, "one probe a period, each panicking");
    assert!(rig.client.is_open(), "a panicking probe closes nothing");

    panicking.store(false, Ordering::SeqCst);
    advance_settled(Duration::from_millis(1_000)).await;
    assert!(!rig.client.is_open(), "the next probe is answered and closes it");
    assert_eq!(rig.send().await.map(|r| r.status), Ok(200));
}

/// A name with no address at boot opens the breaker once its lookup finds
/// nothing, so requests fail at once; a name that resolves leaves it closed.
#[tokio::test(start_paused = true)]
async fn a_name_unresolved_at_boot_opens_the_breaker_until_a_probe_answers() {
    let rig = Rig::new("boot-unresolved:8080", ClientSettings::default()).await;
    rig.resolver.unpoint("boot-unresolved:8080");
    rig.client.trip_unless_resolved();
    settle().await;
    assert!(rig.client.is_open(), "the boot lookup found nothing");
    assert_eq!(rig.send().await, Err(AttemptError::BreakerOpen), "at once, nothing sent");

    rig.resolver.point("boot-unresolved:8080", [rig.dst]);
    advance_settled(Duration::from_millis(1_050)).await;
    assert!(!rig.client.is_open(), "the probe looked it up and was answered");

    let resolved = Rig::new("boot-resolved:8080", ClientSettings::default()).await;
    resolved.client.trip_unless_resolved();
    settle().await;
    assert!(!resolved.client.is_open());
}

// ── the probe's reach ───────────────────────────────────────────────────────

/// A request answered at the rig's address, so the name's addresses are
/// kept; then the breaker's window passes, so the answer no longer counts.
async fn keep_the_address(rig: &Rig) {
    assert_eq!(rig.send().await.map(|r| r.status), Ok(200));
    assert_eq!(rig.client.target().address(), Some(rig.dst));
    let window = BreakerConfig::probing("/health").window;
    advance_settled(window + Duration::from_millis(100)).await;
}

/// The backend at the rig's address refuses three requests, which open the
/// breaker.
async fn open_by_refusals(rig: &Rig) {
    rig.behave(Behaviour::Refuse);
    for _ in 0..3 {
        let _ = rig.client.attempt(HttpRequest::get("/call"), ATTEMPT).await;
    }
    assert!(rig.client.is_open());
}

/// The name stops resolving once its addresses are kept (an empty answer, at
/// once); the backend refuses until 1.5 s after the opening, then answers at
/// the same address. The probe at 1 s fails, the one at 2 s reaches the
/// backend at the kept address and closes the breaker, and requests are
/// served there while the name still resolves to nothing.
#[tokio::test(start_paused = true)]
async fn a_name_that_stops_resolving_keeps_the_probe_on_the_kept_address() {
    let rig = Rig::new("dns-gone:8080", ClientSettings::default()).await;
    keep_the_address(&rig).await;
    rig.resolver.unpoint("dns-gone:8080");
    open_by_refusals(&rig).await;

    advance_settled(Duration::from_millis(1_500)).await;
    assert!(rig.client.is_open(), "the probe at 1 s found the backend refusing");
    rig.net.apply_fault(Fault::Resume { dst: rig.dst });
    advance_settled(Duration::from_millis(550)).await;
    assert!(!rig.client.is_open(), "the probe at 2 s reached the kept address");
    assert_eq!(rig.backend.served("/health"), 1);
    assert_eq!(metrics::breaker_transitions("dns-gone:8080", false), 1);
    assert_eq!(rig.send().await.map(|r| r.status), Ok(200));
    assert_eq!(rig.backend.served("/call"), 2, "the answer before the opening, and this one");
}

/// The name service hangs once the name's addresses are kept, and the
/// backend answers again at the same address right after the opening: a
/// probe reaches it within one probe period and one lookup bound, and closes
/// the breaker.
#[tokio::test(start_paused = true)]
async fn a_hanging_name_service_keeps_the_probe_on_the_kept_address() {
    let rig = Rig::new("dns-hung:8080", ClientSettings::default()).await;
    keep_the_address(&rig).await;
    rig.resolver.slow(Duration::from_secs(30));
    open_by_refusals(&rig).await;
    rig.net.apply_fault(Fault::Resume { dst: rig.dst });

    let probe_period = BreakerConfig::probing("/health").probe_period;
    advance_settled(probe_period + LOOKUP_BOUND + Duration::from_millis(50)).await;
    assert!(!rig.client.is_open(), "a probe reached the kept address");
    assert_eq!(rig.backend.served("/health"), 1);
    assert_eq!(rig.send().await.map(|r| r.status), Ok(200));
}

/// The backend moves: its name resolves to a new address, served there,
/// while the old one refuses. The first probe after the opening reaches the
/// new address and closes the breaker, and the next request is served there.
#[tokio::test(start_paused = true)]
async fn a_moved_backend_is_reached_by_the_first_probe() {
    let rig = Rig::new("moved:8080", ClientSettings::default()).await;
    let moved: SocketAddr = "192.0.2.2:8080".parse().unwrap();
    let mover = Backend::answering(200);
    let _moved = rig.net.serve(moved, mover.clone()).await.unwrap();
    keep_the_address(&rig).await;
    rig.resolver.point("moved:8080", [moved]);
    open_by_refusals(&rig).await;

    advance_settled(Duration::from_millis(1_050)).await;
    assert!(!rig.client.is_open(), "the first probe reached the new address");
    assert_eq!(mover.served("/health"), 1);
    assert_eq!(rig.send().await.map(|r| r.status), Ok(200));
    assert_eq!(mover.served("/call"), 1, "served at the new address");
}

/// The backend moves while the breaker is open: the probe at 1 s fails on
/// the old address, which refuses; at 1.5 s the name resolves to a new
/// address that answers. The probe at 2 s reaches the new address and closes
/// the breaker, and the next request is served there.
#[tokio::test(start_paused = true)]
async fn a_backend_that_moves_while_open_is_reached_by_the_next_probe() {
    let rig = Rig::new("moved-open:8080", ClientSettings::default()).await;
    let moved: SocketAddr = "192.0.2.3:8080".parse().unwrap();
    let mover = Backend::answering(200);
    let _moved = rig.net.serve(moved, mover.clone()).await.unwrap();
    keep_the_address(&rig).await;
    open_by_refusals(&rig).await;

    advance_settled(Duration::from_millis(1_500)).await;
    assert!(rig.client.is_open(), "the probe at 1 s found the old address refusing");
    rig.resolver.point("moved-open:8080", [moved]);
    advance_settled(Duration::from_millis(550)).await;
    assert!(!rig.client.is_open(), "the probe at 2 s reached the new address");
    assert_eq!(mover.served("/health"), 1);
    assert_eq!(metrics::breaker_transitions("moved-open:8080", false), 1);
    assert_eq!(rig.send().await.map(|r| r.status), Ok(200));
    assert_eq!(mover.served("/call"), 1, "served at the new address");
}

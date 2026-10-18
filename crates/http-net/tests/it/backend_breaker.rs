//! The backend breaker under traffic shapes the window rule must survive: a
//! hung backend under a burst, a cap whose waiters never send, and a probe
//! that panics.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use http_net::backend::metrics;
use http_net::{AttemptError, BreakerConfig, ClientSettings, HttpRequest};
use http_net_testkit::{Behaviour, SimulatedHttpNetwork};
use sip_clock::testkit::{advance_settled, settle};

use crate::backend::{Rig, ATTEMPT};

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

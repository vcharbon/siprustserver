use std::sync::Arc;

use super::*;
use crate::limiter::{AdmitOutcome, NoopLimiter};
use crate::router::route_payload::route_result_payload;

/// A worker whose release queue is held: what a fold queued stays
/// readable.
fn releases() -> LimiterWorker {
    let (reentry, _) = mpsc::unbounded_channel();
    let (worker, _tasks) = LimiterWorker::start(
        Arc::new(NoopLimiter),
        &crate::config::B2buaConfig::default(),
        crate::metrics::B2buaMetrics::new(),
        reentry,
    );
    worker.hold_releases();
    worker
}

/// A failover fold whose admit of `[x, y]` came back `outcome`.
fn failover_fold(outcome: AdmitOutcome) -> CallEvent {
    let mut route = crate::decision::test_adapter::route_to("127.0.0.1", 5070);
    route.call_limiter = vec![
        call::LimiterEntry { id: "x".into(), limit: 10 },
        call::LimiterEntry { id: "y".into(), limit: 10 },
    ];
    let entries = ["x", "y"].map(|id| LimiterEntry { id: id.into(), limit: 10 }).to_vec();
    let admitted = AdmitReport { key: "call-1#k".into(), change: 4, entries, outcome };
    let payload = route_result_payload(route, admitted, Some("b-1".into()));
    let to = Caller { call_ref: "call-1".into(), incarnation: "call-1#k".into() };
    internal_event(to, "call-failure-result", "failover", payload, Vec::new())
}

#[tokio::test]
async fn a_route_fold_the_router_cannot_take_releases_its_call() {
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let releases = releases();
    send_admit_fold(&tx, &releases, failover_fold(AdmitOutcome::Admitted));
    assert_eq!(releases.waiting_keys(), ["call-1#k"], "by its key");
}

#[tokio::test]
async fn an_uncounted_route_fold_owing_its_release_releases_its_call() {
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let releases = releases();
    send_admit_fold(&tx, &releases, failover_fold(AdmitOutcome::Unavailable));
    assert_eq!(releases.waiting_keys(), ["call-1#k"], "by its key");
}

#[tokio::test]
async fn a_route_fold_owing_nothing_releases_nothing() {
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let releases = releases();
    send_admit_fold(&tx, &releases, failover_fold(AdmitOutcome::NotSent));
    assert!(releases.waiting_keys().is_empty(), "a call that sent no admit owes nothing");
}

#[tokio::test]
async fn a_delivered_route_fold_keeps_its_state_for_the_call() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let releases = releases();
    send_admit_fold(&tx, &releases, failover_fold(AdmitOutcome::Admitted));
    assert!(releases.waiting_keys().is_empty(), "the call applies it");
    let report = admit_report_of(&rx.recv().await.unwrap()).expect("the fold carries it");
    assert_eq!(report.owed_release(), Some("call-1#k"));
    assert_eq!((report.key.as_str(), report.change), ("call-1#k", 4));
    assert_eq!(report.held().map(|h| h.entries.len()), Some(2));
}

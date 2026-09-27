//! Oracle / "Layer comparison": the `LimiterServer` driven over the simulated
//! HTTP fabric must produce the *same* admit/refuse/refresh decisions as a
//! direct `CallStore` fed the identical op sequence under the identical
//! clock. This proves the serde + routing layer faithfully reflects the core.

use std::sync::Arc;
use std::time::Duration;

use call_limiter::wire::{
    AdmitEntry, AdmitRequest, AdmitResponse, RefreshRequest, RefreshResponse, ReleaseRequest,
};
use call_limiter::{
    AdmitResult, CallStore, LimiterConfig, LimiterMetrics, LimiterServer, RefreshResult,
};
use http_net::{HttpRequest, HttpResponse, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;

fn cfg() -> LimiterConfig {
    LimiterConfig { lease_sec: 10 }
}

fn addr() -> std::net::SocketAddr {
    "10.0.0.1:8080".parse().unwrap()
}

/// Drive one request through the fabric (spawned so the transit-delay sleeps
/// can be advanced) and return the decoded response.
async fn call(net: &SimulatedHttpNetwork, req: HttpRequest) -> HttpResponse {
    let h = tokio::spawn({
        let net = net.clone();
        async move { net.request(addr(), req).await }
    });
    tokio::time::advance(Duration::from_millis(5)).await;
    h.await.unwrap().unwrap()
}

fn entries(ids: &[(&str, i64)]) -> Vec<AdmitEntry> {
    ids.iter().map(|(id, limit)| AdmitEntry { id: (*id).into(), limit: *limit }).collect()
}

#[tokio::test(start_paused = true)]
async fn http_server_matches_direct_core() {
    let clock = Clock::test_at(0);
    // The "system under test": real server logic over the simulated fabric.
    let store = Arc::new(CallStore::new(cfg(), clock.clone()));
    let server = Arc::new(LimiterServer::new(store, LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server.clone()).await.unwrap();

    // The oracle: a separate direct store on the same clock + config.
    let oracle = CallStore::new(cfg(), clock.clone());

    // (call, entries, release_on_refusal): fills A, refuses the third A, a
    // batch A+B refused on A (all-or-none), c2 replacing its set net of its
    // own A, c1 adding B at its cap refused with its set released.
    let admits: Vec<(&str, Vec<AdmitEntry>, bool)> = vec![
        ("c1", entries(&[("A", 2)]), false),
        ("c2", entries(&[("A", 2)]), false),
        ("c3", entries(&[("A", 2)]), false),
        ("c3", entries(&[("A", 2), ("B", 5)]), false),
        ("c2", entries(&[("A", 2), ("B", 1)]), true),
        ("c1", entries(&[("A", 2), ("B", 1)]), true),
        ("c4", entries(&[("B", 5)]), false),
    ];
    for (call_ref, entries, release_on_refusal) in &admits {
        let body = serde_json::to_vec(&AdmitRequest {
            call_ref: (*call_ref).into(),
            entries: entries.clone(),
            release_on_refusal: *release_on_refusal,
        })
        .unwrap();
        let resp = call(&net, HttpRequest::post("/v1/admit", body)).await;
        assert_eq!(resp.status, 200);
        let http: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();
        let direct = oracle.admit(call_ref, entries, *release_on_refusal);
        match (&http, &direct) {
            (AdmitResponse { admitted: true, .. }, AdmitResult::Admitted) => {}
            (
                AdmitResponse { admitted: false, rejected_id: Some(hid), released: false },
                AdmitResult::Rejected { limiter_id: oid },
            ) => assert_eq!(hid, oid, "refused ids agree"),
            _ => panic!("HTTP {http:?} disagrees with core {direct:?}"),
        }
    }

    // Refresh c2 across most of the lease; both stores keep it; c1 lapses.
    tokio::time::advance(Duration::from_secs(8)).await;
    let body = serde_json::to_vec(&RefreshRequest { call_ref: "c2".into(), ids: vec!["A".into()] })
        .unwrap();
    let resp = call(&net, HttpRequest::post("/v1/refresh", body)).await;
    let http: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
    let direct = oracle.refresh("c2", &["A".into()]);
    assert_eq!(
        (http.known, http.reregistered),
        (direct == RefreshResult::Extended, direct == RefreshResult::Reregistered),
        "refresh outcomes agree"
    );
    tokio::time::advance(Duration::from_secs(3)).await;
    assert_eq!(server.store().sweep_now(), oracle.sweep_now(), "the same sets lapsed");

    // Release c2 on both; both free the slot identically.
    let body = serde_json::to_vec(&ReleaseRequest { call_ref: "c2".into() }).unwrap();
    let resp = call(&net, HttpRequest::post("/v1/release", body)).await;
    assert_eq!(resp.status, 200);
    oracle.release("c2");

    for id in ["A", "B"] {
        assert_eq!(server.store().held(id), oracle.held(id), "counts on {id} agree");
    }
    assert_eq!(server.store().calls(), oracle.calls());
}

#[tokio::test(start_paused = true)]
async fn metrics_and_health_endpoints() {
    let clock = Clock::test_at(0);
    let store = Arc::new(CallStore::new(cfg(), clock));
    let server = Arc::new(LimiterServer::new(store, LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server).await.unwrap();

    // One admit + one refusal to move the counters.
    let admit = |call_ref: &str| {
        serde_json::to_vec(&AdmitRequest {
            call_ref: call_ref.into(),
            entries: entries(&[("A", 1)]),
            release_on_refusal: false,
        })
        .unwrap()
    };
    let _ = call(&net, HttpRequest::post("/v1/admit", admit("c1"))).await;
    let _ = call(&net, HttpRequest::post("/v1/admit", admit("c2"))).await; // refused (cap 1)

    let health = call(&net, HttpRequest::get("/healthz")).await;
    assert_eq!(health.status, 200);
    assert_eq!(health.body, b"ok\n");

    let metrics = call(&net, HttpRequest::get("/metrics")).await;
    assert_eq!(metrics.status, 200);
    let text = String::from_utf8(metrics.body).unwrap();
    assert!(text.contains("limiter_admit_total 2"), "{text}");
    assert!(text.contains("limiter_admitted_total 1"), "{text}");
    assert!(text.contains("limiter_rejected_total 1"), "{text}");
    assert!(text.contains("limiter_calls 1"), "{text}");
    assert!(text.contains("limiter_current_total 1"), "{text}");

    let missing = call(&net, HttpRequest::get("/nope")).await;
    assert_eq!(missing.status, 404);
    let malformed = call(&net, HttpRequest::post("/v1/admit", b"{".to_vec())).await;
    assert_eq!(malformed.status, 400);
}

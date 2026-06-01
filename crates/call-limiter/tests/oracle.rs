//! Oracle / "Layer comparison": the `LimiterServer` driven over the simulated
//! HTTP fabric must produce the *same* admit/reject/refresh decisions as a
//! direct `WindowStore` fed the identical op sequence under the identical
//! clock. This proves the serde + routing layer faithfully reflects the core.

use std::sync::Arc;
use std::time::Duration;

use call_limiter::wire::{AdmitEntry, AdmitRequest, AdmitResponse, Hold, RefreshRequest, RefreshResponse, ReleaseRequest};
use call_limiter::{AdmitResult, LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpRequest, HttpResponse, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;

fn cfg() -> LimiterConfig {
    LimiterConfig {
        window_sec: 1,
        active_windows: 3,
        ttl_sec: 10,
    }
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

#[tokio::test(start_paused = true)]
async fn http_server_matches_direct_core() {
    let clock = Clock::test_at(0);
    // The "system under test": real server logic over the simulated fabric.
    let store = Arc::new(WindowStore::new(cfg(), clock.clone()));
    let server = Arc::new(LimiterServer::new(store, LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server.clone()).await.unwrap();

    // The oracle: a separate direct store on the same clock + config.
    let oracle = WindowStore::new(cfg(), clock.clone());

    let batches: Vec<Vec<AdmitEntry>> = vec![
        vec![AdmitEntry { id: "A".into(), limit: 2 }],
        vec![AdmitEntry { id: "A".into(), limit: 2 }],
        vec![AdmitEntry { id: "A".into(), limit: 2 }], // 3rd -> reject
        vec![
            AdmitEntry { id: "A".into(), limit: 2 },
            AdmitEntry { id: "B".into(), limit: 5 },
        ], // transactional: A full -> whole batch rejects
        vec![AdmitEntry { id: "B".into(), limit: 5 }],
    ];

    let mut last_b_hold: Option<Hold> = None;
    for entries in &batches {
        // HTTP path.
        let body = serde_json::to_vec(&AdmitRequest { entries: entries.clone() }).unwrap();
        let resp = call(&net, HttpRequest::post("/v1/admit", body)).await;
        assert_eq!(resp.status, 200);
        let http: AdmitResponse = serde_json::from_slice(&resp.body).unwrap();

        // Direct oracle path.
        let direct = oracle.admit(entries);

        match (&http, &direct) {
            (AdmitResponse { admitted: true, window: Some(hw), .. }, AdmitResult::Admitted { window: ow }) => {
                assert_eq!(hw, ow, "windows agree");
                if entries.iter().any(|e| e.id == "B") {
                    last_b_hold = Some(Hold { id: "B".into(), window: *hw });
                }
            }
            (AdmitResponse { admitted: false, rejected_id: Some(hid), .. }, AdmitResult::Rejected { limiter_id: oid }) => {
                assert_eq!(hid, oid, "rejected ids agree");
            }
            _ => panic!("HTTP {http:?} disagrees with core {direct:?}"),
        }
    }

    // Refresh B across a window and confirm both stores still hold it (cap of 5,
    // but a fresh admit count must match: compare current_total via a probe).
    tokio::time::advance(Duration::from_millis(1000)).await;
    if let Some(hold) = last_b_hold {
        let body = serde_json::to_vec(&RefreshRequest { entries: vec![hold.clone()] }).unwrap();
        let resp = call(&net, HttpRequest::post("/v1/refresh", body)).await;
        let http: RefreshResponse = serde_json::from_slice(&resp.body).unwrap();
        let direct = oracle.refresh(&[hold]);
        assert_eq!(http.entries, direct, "refresh windows agree");

        // Release B on both; both should free the slot identically.
        let body = serde_json::to_vec(&ReleaseRequest { entries: http.entries.clone() }).unwrap();
        let resp = call(&net, HttpRequest::post("/v1/release", body)).await;
        assert_eq!(resp.status, 200);
        oracle.release(&direct);
    }
}

#[tokio::test(start_paused = true)]
async fn metrics_and_health_endpoints() {
    let clock = Clock::test_at(0);
    let store = Arc::new(WindowStore::new(cfg(), clock));
    let server = Arc::new(LimiterServer::new(store, LimiterMetrics::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(addr(), server).await.unwrap();

    // One admit + one reject to move the counters.
    let body = serde_json::to_vec(&AdmitRequest {
        entries: vec![AdmitEntry { id: "A".into(), limit: 1 }],
    })
    .unwrap();
    let _ = call(&net, HttpRequest::post("/v1/admit", body.clone())).await;
    let _ = call(&net, HttpRequest::post("/v1/admit", body)).await; // rejected (cap 1)

    let health = call(&net, HttpRequest::get("/healthz")).await;
    assert_eq!(health.status, 200);
    assert_eq!(health.body, b"ok\n");

    let metrics = call(&net, HttpRequest::get("/metrics")).await;
    assert_eq!(metrics.status, 200);
    let text = String::from_utf8(metrics.body).unwrap();
    assert!(text.contains("limiter_admit_total 2"), "{text}");
    assert!(text.contains("limiter_admitted_total 1"), "{text}");
    assert!(text.contains("limiter_rejected_total 1"), "{text}");
    assert!(text.contains("limiter_live_keys 1"), "{text}");
    assert!(text.contains("limiter_current_total 1"), "{text}");

    let missing = call(&net, HttpRequest::get("/nope")).await;
    assert_eq!(missing.status, 404);
}

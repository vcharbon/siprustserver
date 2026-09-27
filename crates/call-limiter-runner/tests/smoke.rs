//! Real-transport smoke test: bind the `LimiterServer` on a real loopback TCP
//! port via `RealHttpNetwork` and drive admit/refuse/release/metrics over a
//! real pooled `reqwest` client. Runs on a real (non-paused) runtime.

use std::sync::Arc;

use call_limiter::wire::{AdmitEntry, AdmitRequest, AdmitResponse, ReleaseRequest};
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{HttpRequest, HttpTransport, RealHttpNetwork};
use sip_clock::Clock;

#[tokio::test]
async fn real_http_admit_reject_release_metrics() {
    let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::system()));
    let server = Arc::new(LimiterServer::new(store, LimiterMetrics::new()));
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), server).await.unwrap();
    let dst = handle.local_addr();

    let admit = |key: &str| {
        serde_json::to_vec(&AdmitRequest {
            key: key.into(),
            entries: vec![AdmitEntry { id: "trunk-A".into(), limit: 1 }],
            release_on_refusal: false,
        })
        .unwrap()
    };

    // The first call is admitted.
    let r1 = net.request(dst, HttpRequest::post("/v1/admit", admit("c1"))).await.unwrap();
    assert_eq!(r1.status, 200);
    let a1: AdmitResponse = serde_json::from_slice(&r1.body).unwrap();
    assert!(a1.admitted);

    // A second call on the same id (cap 1) is refused — pooled connection reused.
    let r2 = net.request(dst, HttpRequest::post("/v1/admit", admit("c2"))).await.unwrap();
    let a2: AdmitResponse = serde_json::from_slice(&r2.body).unwrap();
    assert!(!a2.admitted);
    assert_eq!(a2.rejected_id.as_deref(), Some("trunk-A"));

    // Release the first call, then the second is admitted.
    let rel = serde_json::to_vec(&ReleaseRequest { keys: vec!["c1".into()] }).unwrap();
    let rr = net.request(dst, HttpRequest::post("/v1/release", rel)).await.unwrap();
    assert_eq!(rr.status, 200);

    let r3 = net.request(dst, HttpRequest::post("/v1/admit", admit("c2"))).await.unwrap();
    let a3: AdmitResponse = serde_json::from_slice(&r3.body).unwrap();
    assert!(a3.admitted, "slot freed by release");

    // Metrics + health endpoints answer.
    let m = net.request(dst, HttpRequest::get("/metrics")).await.unwrap();
    assert_eq!(m.status, 200);
    let text = String::from_utf8(m.body).unwrap();
    assert!(text.contains("limiter_admit_total 3"), "{text}");
    assert!(text.contains("limiter_rejected_total 1"), "{text}");

    let h = net.request(dst, HttpRequest::get("/healthz")).await.unwrap();
    assert_eq!(h.status, 200);
}

#[tokio::test]
async fn real_http_connect_error_when_no_server() {
    let net = RealHttpNetwork::new();
    // Nothing bound here; reqwest should fail to connect.
    let err = net
        .request("127.0.0.1:1".parse().unwrap(), HttpRequest::get("/healthz"))
        .await
        .unwrap_err();
    // Either a connect error or an io error — both map to fail-open upstream.
    let _ = err;
}

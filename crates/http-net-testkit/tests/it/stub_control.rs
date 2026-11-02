//! The stub's control service on the simulated fabric: a set of mappings
//! registered under a tag answers on the listeners, the record read by set
//! and by time window, a set cleared with its record, a shutdown asked for,
//! and the record bounded.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http_net::{HttpRequest, HttpResponse, HttpServerHandle, HttpTransport};
use http_net_testkit::stub::{Stub, StubAnswer, StubMapping};
use http_net_testkit::SimulatedHttpNetwork;

fn served() -> SocketAddr {
    "10.0.0.7:8080".parse().unwrap()
}

fn control() -> SocketAddr {
    "10.0.0.9:8080".parse().unwrap()
}

async fn serve(stub: &Stub) -> (SimulatedHttpNetwork, Vec<Box<dyn HttpServerHandle>>) {
    let net = SimulatedHttpNetwork::new();
    let a = net.serve(served(), stub.listener("a")).await.unwrap();
    let c = net.serve(control(), stub.control()).await.unwrap();
    (net, vec![a, c])
}

fn default() -> StubAnswer {
    StubAnswer::respond(200, br#"{"valid":false}"#.to_vec())
}

fn set_body(tag: &str, path: &str, body: &str) -> Vec<u8> {
    let mappings =
        vec![StubMapping::new("GET", path, StubAnswer::respond(200, body.as_bytes().to_vec()))
            .tagged(tag)];
    serde_json::to_vec(&mappings).unwrap()
}

async fn put(net: &SimulatedHttpNetwork, path: &str, body: Vec<u8>) -> HttpResponse {
    let req = HttpRequest { method: "PUT".into(), path: path.into(), headers: Vec::new(), body };
    net.request(control(), req).await.unwrap()
}

async fn delete(net: &SimulatedHttpNetwork, path: &str) -> HttpResponse {
    let req = HttpRequest {
        method: "DELETE".into(),
        path: path.into(),
        headers: Vec::new(),
        body: Vec::new(),
    };
    net.request(control(), req).await.unwrap()
}

async fn records(net: &SimulatedHttpNetwork, query: &str) -> Vec<serde_json::Value> {
    let resp = net.request(control(), HttpRequest::get(format!("/records{query}"))).await.unwrap();
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    serde_json::from_slice(&resp.body).unwrap()
}

fn now_ms() -> u64 {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_registered_set_answers_on_the_listeners_until_it_is_cleared() {
    let stub = Stub::new(Vec::new(), default());
    let (net, _h) = serve(&stub).await;

    let before = net.request(served(), HttpRequest::get("/p")).await.unwrap();
    assert_eq!(before.body, br#"{"valid":false}"#, "nothing registered: the default");

    assert_eq!(put(&net, "/sets/batch-1", set_body("cell-1", "/p", "one")).await.status, 204);
    let hit = net.request(served(), HttpRequest::get("/p")).await.unwrap();
    assert_eq!(hit.body, b"one");

    assert_eq!(put(&net, "/sets/batch-1", set_body("cell-1", "/p", "two")).await.status, 204);
    let replaced = net.request(served(), HttpRequest::get("/p")).await.unwrap();
    assert_eq!(replaced.body, b"two", "a set registered again replaces its mappings");

    assert_eq!(delete(&net, "/sets/batch-1").await.status, 204);
    let after = net.request(served(), HttpRequest::get("/p")).await.unwrap();
    assert_eq!(after.body, br#"{"valid":false}"#);
}

#[tokio::test(start_paused = true)]
async fn the_record_reads_by_set_and_by_time_window() {
    let stub = Stub::new(Vec::new(), default());
    let (net, _h) = serve(&stub).await;
    put(&net, "/sets/one", set_body("c1", "/one", "1")).await;
    put(&net, "/sets/two", set_body("c2", "/two", "2")).await;
    net.request(served(), HttpRequest::get("/one?k=v")).await.unwrap();
    net.request(served(), HttpRequest::get("/two")).await.unwrap();
    net.request(served(), HttpRequest::get("/other")).await.unwrap();

    let one = records(&net, "?set=one").await;
    assert_eq!(one.len(), 1);
    assert_eq!(
        (one[0]["path"].as_str(), one[0]["set"].as_str(), one[0]["tag"].as_str()),
        (Some("/one"), Some("one"), Some("c1"))
    );
    assert_eq!(records(&net, "").await.len(), 3, "no filter: every request");
    assert_eq!(records(&net, &format!("?since_ms={}", now_ms() + 60_000)).await.len(), 0);
    assert_eq!(records(&net, "?since_ms=0&until_ms=1").await.len(), 0);
    assert_eq!(
        records(&net, &format!("?since_ms=0&until_ms={}", now_ms() + 60_000)).await.len(),
        3
    );
}

#[tokio::test(start_paused = true)]
async fn clearing_a_set_drops_its_record_and_keeps_the_rest() {
    let stub = Stub::new(Vec::new(), default());
    let (net, _h) = serve(&stub).await;
    put(&net, "/sets/one", set_body("c1", "/one", "1")).await;
    net.request(served(), HttpRequest::get("/one")).await.unwrap();
    net.request(served(), HttpRequest::get("/other")).await.unwrap();

    delete(&net, "/sets/one").await;
    let left = records(&net, "").await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0]["path"], "/other");
}

#[tokio::test(start_paused = true)]
async fn a_request_the_control_does_not_know_is_refused() {
    let stub = Stub::new(Vec::new(), default());
    let (net, _h) = serve(&stub).await;
    assert_eq!(put(&net, "/sets/x", b"not json".to_vec()).await.status, 400);
    assert_eq!(put(&net, "/sets/", b"[]".to_vec()).await.status, 404);
    assert_eq!(net.request(control(), HttpRequest::get("/nothing")).await.unwrap().status, 404);
    assert_eq!(records_status(&net, "?since_ms=soon").await, 400);
}

async fn records_status(net: &SimulatedHttpNetwork, query: &str) -> u16 {
    net.request(control(), HttpRequest::get(format!("/records{query}"))).await.unwrap().status
}

#[tokio::test(start_paused = true)]
async fn the_health_states_the_sets_and_records() {
    let stub = Stub::new(Vec::new(), default());
    let (net, _h) = serve(&stub).await;
    put(&net, "/sets/one", set_body("c1", "/one", "1")).await;
    net.request(served(), HttpRequest::get("/one")).await.unwrap();
    let health = net.request(control(), HttpRequest::get("/health")).await.unwrap();
    assert_eq!(health.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&health.body).unwrap();
    assert_eq!((body["sets"].as_u64(), body["records"].as_u64()), (Some(1), Some(1)));
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_asked_for_ends_the_stub() {
    let stub = Stub::new(Vec::new(), default());
    let (net, _h) = serve(&stub).await;
    let stopped = tokio::spawn({
        let stub = stub.clone();
        async move { stub.stopped().await }
    });
    let asked = net.request(control(), HttpRequest::post("/shutdown", Vec::new())).await.unwrap();
    assert_eq!(asked.status, 202);
    tokio::time::timeout(Duration::from_secs(1), stopped).await.unwrap().unwrap();
    let after = net.request(served(), HttpRequest::get("/p")).await;
    assert!(after.is_err(), "a request after the shutdown is closed");
}

#[tokio::test(start_paused = true)]
async fn the_record_keeps_its_latest_requests_within_its_bound() {
    let stub = Stub::new(Vec::new(), default()).with_record_cap(2);
    let (net, _h) = serve(&stub).await;
    for path in ["/1", "/2", "/3"] {
        net.request(served(), HttpRequest::get(path)).await.unwrap();
    }
    let kept: Vec<String> = stub.records().into_iter().map(|r| r.path).collect();
    assert_eq!(kept, ["/2", "/3"]);
}

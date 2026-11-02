//! `Stub` on the simulated fabric under a paused clock: its mappings, its
//! answers (bytes, delays, a close), the default for an unmapped request,
//! the shutdown that ends every pending answer, and its record.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http_net::failures::FailureCause;
use http_net::{HttpRequest, HttpServerHandle, HttpTransport};
use http_net_testkit::stub::{Stub, StubAnswer, StubMapping, StubQuery, StubSpec};
use http_net_testkit::SimulatedHttpNetwork;
use tokio::time::Instant;

fn first() -> SocketAddr {
    "10.0.0.7:8080".parse().unwrap()
}

fn second() -> SocketAddr {
    "10.0.0.8:8080".parse().unwrap()
}

/// `stub` served on the fabric: listener `a` at [`first`], `b` at [`second`].
async fn serve(stub: &Stub) -> (SimulatedHttpNetwork, Vec<Box<dyn HttpServerHandle>>) {
    let net = SimulatedHttpNetwork::new();
    let a = net.serve(first(), stub.listener("a")).await.unwrap();
    let b = net.serve(second(), stub.listener("b")).await.unwrap();
    (net, vec![a, b])
}

fn get(path: &str) -> HttpRequest {
    HttpRequest::get(path)
}

fn json_200(body: &str) -> StubAnswer {
    StubAnswer::respond(200, body.as_bytes().to_vec()).header("content-type", "application/json")
}

#[tokio::test(start_paused = true)]
async fn an_exact_match_answers_its_mapping() {
    let stub = Stub::new(
        vec![StubMapping::new("GET", "/v1/check", json_200(r#"{"ok":true}"#))
            .query(StubQuery::exact("x=1&y=2"))],
        StubAnswer::respond(404, Vec::new()),
    );
    let (net, _h) = serve(&stub).await;

    let hit = net.request(first(), get("/v1/check?x=1&y=2")).await.unwrap();
    assert_eq!(hit.status, 200);
    assert_eq!(hit.body, br#"{"ok":true}"#);
    assert!(hit.headers.contains(&("content-type".into(), "application/json".into())));

    let reordered = net.request(first(), get("/v1/check?y=2&x=1")).await.unwrap();
    assert_eq!(reordered.status, 404, "an exact query is the query string as sent");
    let other_method = net.request(first(), HttpRequest::post("/v1/check?x=1&y=2", vec![])).await;
    assert_eq!(other_method.unwrap().status, 404);
}

#[tokio::test(start_paused = true)]
async fn query_parameters_match_in_any_order() {
    let stub = Stub::new(
        vec![StubMapping::new("GET", "/v1/check", json_200("{}"))
            .query(StubQuery::params([("tn", "331"), ("id", "a b")]))],
        StubAnswer::respond(404, Vec::new()),
    );
    let (net, _h) = serve(&stub).await;

    for target in ["/v1/check?tn=331&id=a%20b", "/v1/check?id=a+b&tn=331"] {
        let resp = net.request(first(), get(target)).await.unwrap();
        assert_eq!(resp.status, 200, "{target}");
    }
    for target in ["/v1/check?tn=331", "/v1/check?tn=331&id=a%20b&extra=1", "/v1/check"] {
        let resp = net.request(first(), get(target)).await.unwrap();
        assert_eq!(resp.status, 404, "{target}: the parameters are all and only those stated");
    }
}

#[tokio::test(start_paused = true)]
async fn a_mapping_on_one_listener_does_not_answer_on_another() {
    let stub = Stub::new(
        vec![StubMapping::new("GET", "/p", json_200("{}")).on_listener("b")],
        StubAnswer::respond(404, Vec::new()),
    );
    let (net, _h) = serve(&stub).await;

    assert_eq!(net.request(first(), get("/p")).await.unwrap().status, 404);
    assert_eq!(net.request(second(), get("/p")).await.unwrap().status, 200);
}

#[tokio::test(start_paused = true)]
async fn a_body_is_served_as_raw_bytes() {
    let raw = vec![b'{', 0xff, 0xfe, b'"', 0x00, b'n', b'o', b't'];
    let stub = Stub::new(
        vec![StubMapping::new("GET", "/raw", StubAnswer::respond(200, raw.clone()))],
        StubAnswer::respond(404, Vec::new()),
    );
    let (net, _h) = serve(&stub).await;

    let resp = net.request(first(), get("/raw")).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, raw, "neither validated nor re-encoded");
}

#[tokio::test(start_paused = true)]
async fn an_answer_waits_its_delay() {
    let stub = Stub::new(
        vec![StubMapping::new("GET", "/slow", json_200("{}").delayed(Duration::from_millis(300)))],
        StubAnswer::respond(404, Vec::new()),
    );
    let (net, _h) = serve(&stub).await;

    let sent = Instant::now();
    let resp = net.request(first(), get("/slow")).await.unwrap();
    let took = sent.elapsed();
    assert_eq!(resp.status, 200);
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_millis(310), "the delay plus two transits: {took:?}");
}

#[tokio::test(start_paused = true)]
async fn a_long_delay_holds_until_the_caller_gives_up() {
    let stub = Stub::new(
        vec![StubMapping::new("GET", "/hang", json_200("{}").delayed(Duration::from_secs(5)))],
        StubAnswer::respond(404, Vec::new()),
    );
    let (net, _h) = serve(&stub).await;

    let budget = Duration::from_millis(200);
    let given_up = tokio::time::timeout(budget, net.request(first(), get("/hang"))).await;
    assert!(given_up.is_err(), "no answer within the caller's budget");
}

#[tokio::test(start_paused = true)]
async fn shutdown_ends_every_pending_answer_with_a_close() {
    let stub = Stub::new(
        vec![StubMapping::new("GET", "/hang", json_200("{}").delayed(Duration::from_secs(5)))],
        StubAnswer::respond(404, Vec::new()),
    );
    let (net, _h) = serve(&stub).await;

    let pending = tokio::spawn({
        let net = net.clone();
        async move { net.request(first(), get("/hang")).await }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let stopped = Instant::now();
    stub.shutdown();
    let ended = pending.await.unwrap().unwrap_err();
    assert_eq!(ended.cause, FailureCause::Eof);
    assert!(
        stopped.elapsed() < Duration::from_millis(10),
        "ended at the shutdown, not at the delay"
    );

    let later = net.request(first(), get("/hang")).await.unwrap_err();
    assert_eq!(later.cause, FailureCause::Eof, "a request after the shutdown is closed too");
}

#[tokio::test(start_paused = true)]
async fn a_close_drops_the_connection_without_a_response() {
    let stub = Stub::new(
        vec![StubMapping::new(
            "GET",
            "/close",
            StubAnswer::close().delayed(Duration::from_millis(50)),
        )],
        StubAnswer::respond(404, Vec::new()),
    );
    let (net, _h) = serve(&stub).await;

    let sent = Instant::now();
    let err = net.request(first(), get("/close")).await.unwrap_err();
    assert_eq!(err.cause, FailureCause::Eof);
    assert!(sent.elapsed() >= Duration::from_millis(50));
}

#[tokio::test(start_paused = true)]
async fn an_unmapped_request_gets_the_default_and_is_recorded() {
    let stub = Stub::new(
        vec![StubMapping::new("GET", "/known", json_200("{}")).tagged("k")],
        StubAnswer::respond(418, b"unmapped".to_vec()),
    );
    let (net, _h) = serve(&stub).await;

    let resp = net.request(first(), get("/unknown?q=1")).await.unwrap();
    assert_eq!(resp.status, 418);
    assert_eq!(resp.body, b"unmapped");

    let records = stub.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].path, "/unknown");
    assert_eq!(records[0].query.as_deref(), Some("q=1"));
    assert!(!records[0].matched);
    assert_eq!(records[0].tag, None);
}

#[tokio::test(start_paused = true)]
async fn every_request_is_recorded_with_its_time_listener_and_mapping() {
    let stub = Stub::new(
        vec![
            StubMapping::new("GET", "/a", json_200("{}")).tagged("first"),
            StubMapping::new("POST", "/b", StubAnswer::close()).tagged("second"),
        ],
        StubAnswer::respond(404, Vec::new()),
    );
    let (net, _h) = serve(&stub).await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    net.request(first(), get("/a?x=1").header("api-key", "k1")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = net.request(second(), HttpRequest::post("/b", b"payload".to_vec())).await;

    let records = stub.records();
    assert_eq!(records.len(), 2);
    let (a, b) = (&records[0], &records[1]);
    assert_eq!((a.listener.as_str(), a.method.as_str(), a.path.as_str()), ("a", "GET", "/a"));
    assert_eq!(a.query.as_deref(), Some("x=1"));
    assert_eq!(a.headers, vec![("api-key".to_string(), "k1".to_string())]);
    assert_eq!((a.matched, a.tag.as_deref()), (true, Some("first")));
    assert_eq!(
        (b.listener.as_str(), b.method.as_str(), b.body.as_slice()),
        ("b", "POST", &b"payload"[..])
    );
    assert_eq!((b.matched, b.tag.as_deref()), (true, Some("second")));
    // Received after one transit each: 101 ms, then 100 ms and a round trip later.
    assert_eq!(a.at, Duration::from_millis(101));
    assert_eq!(b.at, Duration::from_millis(203));
}

#[tokio::test(start_paused = true)]
async fn a_spec_reads_from_json() {
    let spec: StubSpec = serde_json::from_str(
        r#"{
          "listeners": { "a": "127.0.0.1:9001" },
          "default": { "respond": { "status": 404 } },
          "mappings": [
            { "tag": "t", "listener": "a", "method": "GET", "path": "/v1/x",
              "query": { "params": [["tn", "1"], ["id", "k"]] },
              "answer": { "delay_ms": 20, "respond": { "status": 200,
                "headers": [["content-type", "application/json"]], "body_b64": "eyJ2YWxpZCI6dHJ1ZX0=" } } },
            { "method": "GET", "path": "/gone", "answer": { "close": {} } }
          ]
        }"#,
    )
    .unwrap();
    assert_eq!(spec.listeners["a"], "127.0.0.1:9001".parse::<SocketAddr>().unwrap());
    let stub = Stub::from_spec(&spec);
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(first(), stub.listener("a")).await.unwrap();

    let sent = Instant::now();
    let resp = net.request(first(), get("/v1/x?id=k&tn=1")).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, br#"{"valid":true}"#);
    assert!(sent.elapsed() >= Duration::from_millis(20));
    assert_eq!(net.request(first(), get("/gone")).await.unwrap_err().cause, FailureCause::Eof);
    assert_eq!(net.request(first(), get("/v1/x?tn=1")).await.unwrap().status, 404);
}

#[tokio::test(start_paused = true)]
async fn a_record_serializes_its_body_as_base64() {
    let stub = Stub::new(Vec::new(), StubAnswer::respond(404, Vec::new()));
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(first(), stub.listener("a")).await.unwrap();
    net.request(first(), HttpRequest::post("/p?q=1", vec![0xff, b'a'])).await.unwrap();

    let line = serde_json::to_value(&stub.records()[0]).unwrap();
    assert_eq!(line["listener"], "a");
    assert_eq!(line["path"], "/p");
    assert_eq!(line["query"], "q=1");
    assert_eq!(line["body_b64"], "/2E=");
    assert_eq!(line["matched"], false);
    assert!(line["at_ms"].is_u64());
    assert!(line["at_unix_ms"].is_u64());
}

/// A service handed to a transport outlives the test's handle on the stub.
#[tokio::test(start_paused = true)]
async fn a_listener_keeps_serving_after_the_stub_handle_is_dropped() {
    let stub = Stub::new(vec![StubMapping::new("GET", "/a", json_200("{}"))], StubAnswer::close());
    let listener = stub.listener("a");
    drop(stub);
    let net = SimulatedHttpNetwork::new();
    let _h = net.serve(first(), Arc::clone(&listener)).await.unwrap();
    assert_eq!(net.request(first(), get("/a")).await.unwrap().status, 200);
}

//! `Stub` behind the real transport (hyper server, reqwest client, loopback
//! sockets, real clock): the query as the wire carries it, a close, and a
//! shutdown ending a pending answer.

use std::time::{Duration, Instant};

use http_net::failures::FailureCause;
use http_net::{HttpRequest, HttpTransport, RealHttpNetwork};
use http_net_testkit::stub::{Stub, StubAnswer, StubMapping, StubQuery};

fn stub() -> Stub {
    Stub::new(
        vec![
            StubMapping::new("GET", "/v1/check", StubAnswer::respond(200, br#"{"ok":1}"#.to_vec()))
                .query(StubQuery::params([("b", "2"), ("a", "x y")]))
                .tagged("check"),
            StubMapping::new("GET", "/close", StubAnswer::close()),
            StubMapping::new(
                "GET",
                "/hang",
                StubAnswer::respond(200, Vec::new()).delayed(Duration::from_secs(30)),
            ),
        ],
        StubAnswer::respond(404, Vec::new()),
    )
}

#[tokio::test]
async fn a_query_on_the_wire_matches_in_any_order() {
    let stub = stub();
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), stub.listener("a")).await.unwrap();

    let resp = net.request(handle.local_addr(), HttpRequest::get("/v1/check?a=x%20y&b=2")).await;
    assert_eq!(resp.unwrap().status, 200);
    let records = stub.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].query.as_deref(), Some("a=x%20y&b=2"));
    assert_eq!(records[0].tag.as_deref(), Some("check"));
}

#[tokio::test]
async fn a_close_reaches_the_client_as_a_closed_connection() {
    let stub = stub();
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), stub.listener("a")).await.unwrap();

    let err = net.request(handle.local_addr(), HttpRequest::get("/close")).await.unwrap_err();
    assert!(matches!(err.cause, FailureCause::Eof | FailureCause::ConnReset), "{err:?}");
}

#[tokio::test]
async fn a_shutdown_ends_a_pending_answer_on_the_wire() {
    let stub = stub();
    let net = std::sync::Arc::new(RealHttpNetwork::new());
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), stub.listener("a")).await.unwrap();
    let dst = handle.local_addr();

    let pending = tokio::spawn({
        let net = std::sync::Arc::clone(&net);
        async move { net.request(dst, HttpRequest::get("/hang")).await }
    });
    while stub.records().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let stopped = Instant::now();
    stub.shutdown();
    let err = pending.await.unwrap().unwrap_err();
    assert!(matches!(err.cause, FailureCause::Eof | FailureCause::ConnReset), "{err:?}");
    assert!(
        stopped.elapsed() < Duration::from_secs(5),
        "ended at the shutdown: {:?}",
        stopped.elapsed()
    );
}

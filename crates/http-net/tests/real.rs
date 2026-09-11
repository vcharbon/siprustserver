//! Real-transport (`feature = "real"`) round-trip of the fields the unary model
//! now carries: a query string on the request target, request headers, and
//! response headers. Runs on a real (non-paused) loopback runtime — the wire is
//! hyper server ⇄ pooled reqwest client, so this exercises the actual
//! `path_and_query`/`HeaderMap` mapping, not the in-memory fabric.

#![cfg(feature = "real")]

use std::sync::Arc;

use async_trait::async_trait;
use http_net::failures::{self, FailureCause};
use http_net::{HttpError, HttpRequest, HttpResponse, HttpService, HttpTransport, RealHttpNetwork};

/// Echoes the received path-and-query as the body and reflects the `x-debug`
/// request header back out, alongside a minted trace-id response header.
struct ReflectService;

#[async_trait]
impl HttpService for ReflectService {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        let echoed = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-debug"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        HttpResponse::ok(req.path.into_bytes())
            .header("x-echoed-debug", echoed)
            .header("x-example-trace-id", "trace-1")
    }
}

#[tokio::test]
async fn real_transport_round_trips_query_and_headers() {
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), Arc::new(ReflectService)).await.unwrap();
    let dst = handle.local_addr();

    let req = HttpRequest::get("/routes?debug=true&seed=7").header("x-debug", "on");
    let resp = net.request(dst, req).await.unwrap();

    assert_eq!(resp.status, 200);
    // Query string reached the server via path_and_query and was echoed back.
    assert_eq!(resp.body, b"/routes?debug=true&seed=7");
    // Request header crossed the wire; response headers came back.
    assert!(
        resp.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("x-echoed-debug") && v == "on"),
        "request header not reflected: {:?}",
        resp.headers
    );
    assert!(
        resp.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-example-trace-id") && v == "trace-1"),
        "response header missing: {:?}",
        resp.headers
    );
}

/// A rule-configured callout endpoint may be named anything — `/tls/admit`,
/// `/v1/certificates`. When that backend is down the operator must read
/// `cause="refused"`, not `cause="tls"`: the url is part of the reqwest error's
/// own message, so it must never reach the text classifier. Drives the real
/// client against a closed port, so the whole chain (reqwest → hyper → io) is
/// the one production produces.
#[tokio::test]
async fn a_refused_backend_behind_a_tls_named_path_counts_as_refused() {
    // A closed port: bind to learn a free one, then drop the listener.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dst = listener.local_addr().unwrap();
    drop(listener);

    let net = RealHttpNetwork::new();
    let err = net
        .request(dst, HttpRequest::post("/tls/admit", b"{}".to_vec()))
        .await
        .expect_err("a closed port must not answer");
    assert!(matches!(err, HttpError::Connect(addr) if addr == dst), "{err:?}");

    // The peer label is this test's own ephemeral port, so the process-wide
    // counters are not shared with any other test.
    let peer = dst.to_string();
    assert_eq!(failures::get(&peer, FailureCause::Refused), 1, "{}", failures::prometheus_text());
    assert_eq!(failures::get(&peer, FailureCause::Tls), 0, "{}", failures::prometheus_text());
}

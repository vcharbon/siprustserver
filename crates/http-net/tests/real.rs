//! Real-transport (`feature = "real"`) round-trip of the fields the unary model
//! now carries: a query string on the request target, request headers, and
//! response headers. Runs on a real (non-paused) loopback runtime — the wire is
//! hyper server ⇄ pooled reqwest client, so this exercises the actual
//! `path_and_query`/`HeaderMap` mapping, not the in-memory fabric.

#![cfg(feature = "real")]

use std::sync::Arc;

use async_trait::async_trait;
use http_net::{HttpRequest, HttpResponse, HttpService, HttpTransport, RealHttpNetwork};

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
            .header("x-newkah-trace-id", "trace-1")
    }
}

#[tokio::test]
async fn real_transport_round_trips_query_and_headers() {
    let net = RealHttpNetwork::new();
    let handle = net
        .serve("127.0.0.1:0".parse().unwrap(), Arc::new(ReflectService))
        .await
        .unwrap();
    let dst = handle.local_addr();

    let req = HttpRequest::get("/routes?debug=true&seed=7").header("x-debug", "on");
    let resp = net.request(dst, req).await.unwrap();

    assert_eq!(resp.status, 200);
    // Query string reached the server via path_and_query and was echoed back.
    assert_eq!(resp.body, b"/routes?debug=true&seed=7");
    // Request header crossed the wire; response headers came back.
    assert!(
        resp.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-echoed-debug") && v == "on"),
        "request header not reflected: {:?}",
        resp.headers
    );
    assert!(
        resp.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-newkah-trace-id") && v == "trace-1"),
        "response header missing: {:?}",
        resp.headers
    );
}

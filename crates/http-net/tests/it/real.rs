//! Real-transport (`feature = "real"`) round-trip of the fields the unary model
//! now carries: a query string on the request target, request headers, and
//! response headers. Runs on a real (non-paused) loopback runtime — the wire is
//! hyper server ⇄ pooled reqwest client, so this exercises the actual
//! `path_and_query`/`HeaderMap` mapping, not the in-memory fabric.

#![cfg(feature = "real")]

use std::sync::Arc;

use async_trait::async_trait;
use http_net::failures::{self, FailureCause};
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
    assert_eq!(
        (err.cause, err.peer.as_str()),
        (FailureCause::Refused, &*dst.to_string()),
        "{err:?}"
    );

    // The peer label is this test's own ephemeral port, so the process-wide
    // counters are not shared with any other test.
    let peer = dst.to_string();
    assert_eq!(failures::get(&peer, FailureCause::Refused), 1, "{}", failures::prometheus_text());
    assert_eq!(failures::get(&peer, FailureCause::Tls), 0, "{}", failures::prometheus_text());
}

/// Answers a POST to `/moved` with `302 Found` to `/landing`, and anything
/// sent to `/landing` with `200`.
struct RedirectService;

#[async_trait]
impl HttpService for RedirectService {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        match req.path.as_str() {
            "/moved" => HttpResponse::status(302).header("location", "/landing"),
            _ => HttpResponse::ok(b"landed".to_vec()),
        }
    }
}

/// A unary RPC's answer is the endpoint's own: a 3xx reaches the caller as
/// its status. Following it would re-send a POST as a body-less GET and hand
/// the caller the redirect target's 200 as the acknowledgement.
#[tokio::test]
async fn a_redirect_is_returned_not_followed() {
    let net = RealHttpNetwork::new();
    let handle =
        net.serve("127.0.0.1:0".parse().unwrap(), Arc::new(RedirectService)).await.unwrap();

    let resp = net
        .request(handle.local_addr(), HttpRequest::post("/moved", b"{}".to_vec()))
        .await
        .unwrap();

    assert_eq!((resp.status, resp.body.as_slice()), (302, &b""[..]));
}

/// Answers every request with a body of `len` bytes.
struct SizedService(usize);

#[async_trait]
impl HttpService for SizedService {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        HttpResponse::ok(vec![b'x'; self.0])
    }
}

/// Answers every request with its own body's length.
struct LengthService;

#[async_trait]
impl HttpService for LengthService {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        HttpResponse::ok(req.body.len().to_string().into_bytes())
    }
}

/// A raw HTTP/1.1 peer: reads one request head off each connection and
/// writes `head` then `chunks` copies of `chunk`, the connection left open.
async fn raw_peer(head: &'static str, chunk: Vec<u8>, chunks: usize) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let chunk = chunk.clone();
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buf = [0u8; 4096];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                if stream.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                for _ in 0..chunks {
                    if stream.write_all(&chunk).await.is_err() {
                        return;
                    }
                }
                std::future::pending::<()>().await;
            });
        }
    });
    addr
}

/// A chunk of `len` bytes in chunked transfer coding.
fn chunked(len: usize) -> Vec<u8> {
    let mut c = format!("{len:x}\r\n").into_bytes();
    c.extend(std::iter::repeat_n(b'x', len));
    c.extend_from_slice(b"\r\n");
    c
}

#[tokio::test]
async fn a_response_body_at_the_cap_is_carried() {
    let net = RealHttpNetwork::new();
    let handle = net
        .serve("127.0.0.1:0".parse().unwrap(), Arc::new(SizedService(http_net::MAX_BODY_BYTES)))
        .await
        .unwrap();

    let resp = net.request(handle.local_addr(), HttpRequest::get("/")).await.unwrap();

    assert_eq!((resp.status, resp.body.len()), (200, http_net::MAX_BODY_BYTES));
}

#[tokio::test]
async fn a_response_body_past_the_cap_fails_as_body_too_large() {
    let net = RealHttpNetwork::new();
    let handle = net
        .serve("127.0.0.1:0".parse().unwrap(), Arc::new(SizedService(http_net::MAX_BODY_BYTES + 1)))
        .await
        .unwrap();
    let dst = handle.local_addr();

    let err = net.request(dst, HttpRequest::get("/")).await.expect_err("past the cap");

    assert_eq!(err.cause, FailureCause::BodyTooLarge, "{err:?}");
    assert!(!err.cause.is_retryable());
    assert_eq!(failures::get(&dst.to_string(), FailureCause::BodyTooLarge), 1);
}

/// A declared length past the cap fails on the head: nothing of the body is
/// sent, so a client that waited for it would run to its ceiling.
#[tokio::test]
async fn a_declared_length_past_the_cap_fails_before_the_body() {
    let dst =
        raw_peer("HTTP/1.1 200 OK\r\nContent-Length: 1073741824\r\n\r\n", Vec::new(), 0).await;
    let net = RealHttpNetwork::new();

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        net.request(dst, HttpRequest::get("/")),
    )
    .await
    .expect("refused on the declared length, not at the ceiling")
    .expect_err("past the cap");

    assert_eq!(err.cause, FailureCause::BodyTooLarge, "{err:?}");
}

/// A chunked body declares no length: the cap holds on the bytes streamed.
#[tokio::test]
async fn a_chunked_body_past_the_cap_fails_as_it_streams() {
    const CHUNK: usize = 1024 * 1024;
    let chunks = http_net::MAX_BODY_BYTES / CHUNK + 1;
    let dst =
        raw_peer("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", chunked(CHUNK), chunks)
            .await;
    let net = RealHttpNetwork::new();

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        net.request(dst, HttpRequest::get("/")),
    )
    .await
    .expect("refused on the bytes streamed, not at the ceiling")
    .expect_err("past the cap");

    assert_eq!(err.cause, FailureCause::BodyTooLarge, "{err:?}");
}

#[tokio::test]
async fn a_request_body_past_the_cap_is_answered_413_without_the_service() {
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), Arc::new(LengthService)).await.unwrap();
    let dst = handle.local_addr();

    let at_cap = net
        .request(dst, HttpRequest::post("/", vec![b'x'; http_net::MAX_BODY_BYTES]))
        .await
        .unwrap();
    let past = net
        .request(dst, HttpRequest::post("/", vec![b'x'; http_net::MAX_BODY_BYTES + 1]))
        .await
        .unwrap();

    assert_eq!(
        (at_cap.status, at_cap.body),
        (200, http_net::MAX_BODY_BYTES.to_string().into_bytes())
    );
    assert_eq!(past.status, 413);
}

/// Counts the requests it serves.
#[derive(Default)]
struct CountingService(std::sync::atomic::AtomicUsize);

#[async_trait]
impl HttpService for CountingService {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        HttpResponse::status(204)
    }
}

/// Writes `head` and `body` raw to `dst`, half-closes, and returns every byte
/// the server answers until it closes the connection.
async fn raw_exchange(dst: std::net::SocketAddr, head: &str, body: &[u8]) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(dst).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.shutdown().await.unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(2), stream.read_to_end(&mut answer))
        .await
        .expect("the server closes the connection")
        .unwrap();
    String::from_utf8_lossy(&answer).into_owned()
}

/// A declared length past the cap is answered on the head: the server reads
/// none of the body, so the 413 is not lost to a reset over unread bytes.
#[tokio::test]
async fn a_declared_request_length_past_the_cap_is_answered_413_on_the_head() {
    let net = RealHttpNetwork::new();
    let service = Arc::new(CountingService::default());
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), service.clone()).await.unwrap();

    let answer = raw_exchange(
        handle.local_addr(),
        "POST / HTTP/1.1\r\nHost: peer\r\nContent-Length: 1073741824\r\n\r\n",
        b"",
    )
    .await;

    assert!(answer.starts_with("HTTP/1.1 413"), "{answer:?}");
    assert_eq!(service.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// A request whose body does not arrive whole is never served.
#[tokio::test]
async fn a_truncated_request_body_never_reaches_the_service() {
    let net = RealHttpNetwork::new();
    let service = Arc::new(CountingService::default());
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), service.clone()).await.unwrap();

    let answer = raw_exchange(
        handle.local_addr(),
        "POST / HTTP/1.1\r\nHost: peer\r\nContent-Length: 100\r\n\r\n",
        b"0123456789",
    )
    .await;

    assert_eq!(answer, "");
    assert_eq!(service.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}

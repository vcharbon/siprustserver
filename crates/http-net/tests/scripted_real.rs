//! `ScriptedHttpService` behind the real transport (hyper server, reqwest
//! client, loopback sockets, real clock): the same program served over the
//! wire, and the faults as a peer on a socket sees them.

#![cfg(feature = "real")]

use std::sync::Arc;
use std::time::Duration;

use http_net::scripted::{
    HttpBindings, HttpFindingKind, HttpReifiedStep, HttpReply, HttpRequestMatch, HttpScript,
    ScriptedHttpService,
};
use http_net::{HttpError, HttpRequest, HttpResponse, HttpTransport, RealHttpNetwork};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn ctx(resp: &HttpResponse) -> String {
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    v["ctx"].as_str().unwrap().to_string()
}

fn two_step() -> HttpScript {
    HttpScript::reified(
        HttpRequestMatch::post("/start").contains(r#""cell":"${bind:cell}""#),
        vec![
            HttpReifiedStep::new(
                HttpRequestMatch::post("/start"),
                HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#)
                    .header("content-type", "application/json"),
            ),
            HttpReifiedStep::new(
                HttpRequestMatch::post("/next"),
                HttpReply::respond(200, r#"{"done":"${bind:cell}"}"#),
            ),
        ],
    )
}

fn single(reply: HttpReply) -> HttpScript {
    HttpScript::reified(
        HttpRequestMatch::post("/start"),
        vec![HttpReifiedStep::new(HttpRequestMatch::post("/start"), reply)],
    )
}

/// Write one raw HTTP/1.1 POST and read until the server closes the stream or
/// `budget` runs out (`None`).
async fn raw_post(stream: &mut TcpStream, body: &str, budget: Duration) -> Option<Vec<u8>> {
    let head = format!(
        "POST /start HTTP/1.1\r\nhost: scripted\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut read = Vec::new();
    match tokio::time::timeout(budget, stream.read_to_end(&mut read)).await {
        Ok(Ok(_)) => Some(read),
        // A reset reads as an error: closed all the same, nothing received.
        Ok(Err(_)) => Some(read),
        Err(_) => None,
    }
}

#[tokio::test]
async fn a_two_step_script_is_served_over_real_sockets() {
    let svc = ScriptedHttpService::new();
    let script = svc.add(two_step(), HttpBindings::new().bind("cell", "c1")).unwrap();
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), Arc::new(svc.clone())).await.unwrap();
    let dst = handle.local_addr();

    let first = net.request(dst, HttpRequest::post("/start", br#"{"cell":"c1"}"#.to_vec()));
    let first = first.await.unwrap();
    assert_eq!(first.status, 200);
    assert!(first.headers.iter().any(|(k, v)| k == "content-type" && v == "application/json"));
    let next = format!(r#"{{"ctx":"{}"}}"#, ctx(&first)).into_bytes();

    let a = net.request(dst, HttpRequest::post("/next", next.clone())).await.unwrap();
    let b = net.request(dst, HttpRequest::post("/next", next)).await.unwrap();
    assert_eq!(a.body, br#"{"done":"c1"}"#);
    assert_eq!((a.status, &a.body), (b.status, &b.body), "a retransmit is answered identically");
    assert!(script.verdict().is_green(), "{:?}", script.verdict());
}

#[tokio::test]
async fn an_unmatched_request_is_a_500_over_real_sockets() {
    let svc = ScriptedHttpService::new();
    let script = svc.add(two_step(), HttpBindings::new().bind("cell", "c1")).unwrap();
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), Arc::new(svc.clone())).await.unwrap();

    let resp = net
        .request(handle.local_addr(), HttpRequest::post("/start", br#"{"cell":"c2"}"#.to_vec()))
        .await
        .unwrap();
    assert_eq!(resp.status, 500);
    assert!(svc.findings().iter().any(|f| f.kind == HttpFindingKind::Unmatched));
    assert!(!script.verdict().opened);
}

#[tokio::test]
async fn reset_closes_the_connection_without_a_response() {
    let svc = ScriptedHttpService::new();
    svc.add(single(HttpReply::Reset), HttpBindings::new()).unwrap();
    svc.add(single(HttpReply::Reset), HttpBindings::new()).unwrap();
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), Arc::new(svc.clone())).await.unwrap();
    let dst = handle.local_addr();

    let mut stream = TcpStream::connect(dst).await.unwrap();
    let read = raw_post(&mut stream, "{}", Duration::from_secs(5)).await;
    let read = read.expect("the server closes the connection");
    assert!(read.is_empty(), "no status line: {:?}", String::from_utf8_lossy(&read));

    let err = net.request(dst, HttpRequest::post("/start", b"{}".to_vec())).await.unwrap_err();
    assert!(matches!(err, HttpError::Io { .. }), "the client sees a transport failure: {err:?}");
}

#[tokio::test]
async fn late_answers_after_the_stated_delay_over_real_sockets() {
    let svc = ScriptedHttpService::new();
    svc.add(single(HttpReply::respond(200, "late").late(150)), HttpBindings::new()).unwrap();
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), Arc::new(svc.clone())).await.unwrap();

    let t0 = std::time::Instant::now();
    let resp = net.request(handle.local_addr(), HttpRequest::post("/start", vec![])).await.unwrap();
    assert_eq!(resp.body, b"late");
    assert!(t0.elapsed() >= Duration::from_millis(150), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn dropping_the_server_closes_a_silenced_connection() {
    let svc = ScriptedHttpService::new();
    let script = svc.add(single(HttpReply::Silence), HttpBindings::new()).unwrap();
    let net = RealHttpNetwork::new();
    let handle = net.serve("127.0.0.1:0".parse().unwrap(), Arc::new(svc.clone())).await.unwrap();

    let mut stream = TcpStream::connect(handle.local_addr()).await.unwrap();
    assert!(
        raw_post(&mut stream, "{}", Duration::from_millis(300)).await.is_none(),
        "silence: the connection stays open with no answer"
    );
    assert!(script.verdict().opened);

    drop(handle);
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut rest)).await;
    assert!(closed.is_ok(), "the connection task outlived its server");
    assert!(rest.is_empty());
}

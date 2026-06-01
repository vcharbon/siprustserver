//! `SimulatedHttpNetwork` behaviour: happy-path serve/request, each dst-keyed
//! fault, and the recording decorator. Paused-clock tests use
//! `advance_in_100ms_chunks` so a stalled request's caller-side timeout fires
//! deterministically.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http_net::{
    ExchangeOutcome, Fault, HttpError, HttpRequest, HttpResponse, HttpService, HttpTransport,
    RecordingHttpNetwork, SimulatedHttpNetwork,
};
use sip_clock::testkit::advance_in_100ms_chunks;
use sip_clock::Clock;

fn addr(s: &str) -> std::net::SocketAddr {
    s.parse().unwrap()
}

/// Echoes the request path back as the body, counting how many times it ran.
struct EchoService {
    calls: Arc<AtomicU32>,
}

#[async_trait]
impl HttpService for EchoService {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if req.path == "/boom" {
            return HttpResponse::status(500);
        }
        HttpResponse::ok(req.path.into_bytes())
    }
}

fn echo() -> (Arc<EchoService>, Arc<AtomicU32>) {
    let calls = Arc::new(AtomicU32::new(0));
    (
        Arc::new(EchoService {
            calls: calls.clone(),
        }),
        calls,
    )
}

#[tokio::test(start_paused = true)]
async fn happy_path_request_invokes_handler() {
    let net = SimulatedHttpNetwork::new();
    let (svc, calls) = echo();
    let _server = net.serve(addr("10.0.0.1:8080"), svc).await.unwrap();

    let resp = tokio::spawn({
        let net = net.clone();
        async move { net.request(addr("10.0.0.1:8080"), HttpRequest::post("/hi", vec![])).await }
    });
    // Drive the two transit-delay sleeps.
    advance_in_100ms_chunks(Duration::from_millis(10)).await;
    let resp = resp.await.unwrap().unwrap();

    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"/hi");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn non_2xx_response_is_still_ok() {
    let net = SimulatedHttpNetwork::new();
    let (svc, _calls) = echo();
    let _server = net.serve(addr("10.0.0.1:8080"), svc).await.unwrap();

    let h = tokio::spawn({
        let net = net.clone();
        async move { net.request(addr("10.0.0.1:8080"), HttpRequest::post("/boom", vec![])).await }
    });
    advance_in_100ms_chunks(Duration::from_millis(10)).await;
    let resp = h.await.unwrap().unwrap();
    assert_eq!(resp.status, 500, "app errors are non-2xx responses, not HttpError");
}

#[tokio::test(start_paused = true)]
async fn no_server_is_connect_error() {
    let net = SimulatedHttpNetwork::new();
    let err = net
        .request(addr("10.0.0.9:8080"), HttpRequest::get("/x"))
        .await
        .unwrap_err();
    assert!(matches!(err, HttpError::Connect(_)));
}

#[tokio::test(start_paused = true)]
async fn dropping_the_handle_deregisters() {
    let net = SimulatedHttpNetwork::new();
    let (svc, _calls) = echo();
    let server = net.serve(addr("10.0.0.1:8080"), svc).await.unwrap();
    drop(server);
    let err = net
        .request(addr("10.0.0.1:8080"), HttpRequest::get("/x"))
        .await
        .unwrap_err();
    assert!(matches!(err, HttpError::Connect(_)));
}

#[tokio::test(start_paused = true)]
async fn cut_fails_immediately() {
    let net = SimulatedHttpNetwork::new();
    let (svc, calls) = echo();
    let dst = addr("10.0.0.1:8080");
    let _server = net.serve(dst, svc).await.unwrap();
    net.apply_fault(Fault::Cut { dst });

    let err = net.request(dst, HttpRequest::get("/x")).await.unwrap_err();
    assert!(matches!(err, HttpError::Connect(_)));
    assert_eq!(calls.load(Ordering::Relaxed), 0, "handler never ran");

    // Resume clears the cut.
    net.apply_fault(Fault::Resume { dst });
    let h = tokio::spawn({
        let net = net.clone();
        async move { net.request(dst, HttpRequest::get("/x")).await }
    });
    advance_in_100ms_chunks(Duration::from_millis(10)).await;
    assert!(h.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn error_after_resets_following_delay() {
    let net = SimulatedHttpNetwork::new();
    let (svc, calls) = echo();
    let dst = addr("10.0.0.1:8080");
    let _server = net.serve(dst, svc).await.unwrap();
    net.apply_fault(Fault::ErrorAfter { dst, ms: 50 });

    let h = tokio::spawn({
        let net = net.clone();
        async move { net.request(dst, HttpRequest::get("/x")).await }
    });
    advance_in_100ms_chunks(Duration::from_millis(60)).await;
    let err = h.await.unwrap().unwrap_err();
    assert!(matches!(err, HttpError::Io { .. }));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn stall_hangs_until_caller_times_out() {
    let net = SimulatedHttpNetwork::new();
    let (svc, calls) = echo();
    let dst = addr("10.0.0.1:8080");
    let _server = net.serve(dst, svc).await.unwrap();
    net.apply_fault(Fault::Stall { dst });

    // The caller owns the timeout budget; a stalled request never completes on
    // its own, so the timeout fires when the harness advances.
    let h = tokio::spawn({
        let net = net.clone();
        async move {
            tokio::time::timeout(
                Duration::from_millis(150),
                net.request(dst, HttpRequest::get("/x")),
            )
            .await
        }
    });
    advance_in_100ms_chunks(Duration::from_millis(200)).await;
    let outcome = h.await.unwrap();
    assert!(outcome.is_err(), "request should time out under stall");
    assert_eq!(calls.load(Ordering::Relaxed), 0, "handler never ran while stalled");
}

#[tokio::test(start_paused = true)]
async fn stall_then_resume_delivers() {
    let net = SimulatedHttpNetwork::new();
    let (svc, calls) = echo();
    let dst = addr("10.0.0.1:8080");
    let _server = net.serve(dst, svc).await.unwrap();
    net.apply_fault(Fault::Stall { dst });

    let h = tokio::spawn({
        let net = net.clone();
        async move { net.request(dst, HttpRequest::get("/ok")).await }
    });
    // Let the request reach its stall park.
    advance_in_100ms_chunks(Duration::from_millis(10)).await;
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    net.apply_fault(Fault::Resume { dst });
    advance_in_100ms_chunks(Duration::from_millis(10)).await;
    let resp = h.await.unwrap().unwrap();
    assert_eq!(resp.body, b"/ok");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn delay_fault_raises_transit() {
    let net = SimulatedHttpNetwork::with_transit_delay(1);
    let (svc, _calls) = echo();
    let dst = addr("10.0.0.1:8080");
    let _server = net.serve(dst, svc).await.unwrap();
    net.apply_fault(Fault::Delay { dst, ms: 500 });

    let h = tokio::spawn({
        let net = net.clone();
        async move { net.request(dst, HttpRequest::get("/slow")).await }
    });
    // Two 500 ms transit legs => not done at 200 ms, done by 1100 ms.
    advance_in_100ms_chunks(Duration::from_millis(200)).await;
    assert!(!h.is_finished());
    advance_in_100ms_chunks(Duration::from_millis(900)).await;
    assert!(h.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn recorder_captures_response_and_error() {
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let rec = RecordingHttpNetwork::new(sim.clone(), Clock::test_at(0));
    let (svc, _calls) = echo();
    let dst = addr("10.0.0.1:8080");
    let _server = rec.serve(dst, svc).await.unwrap();

    let h = tokio::spawn({
        let rec = rec.clone();
        async move { rec.request(dst, HttpRequest::post("/hi", b"body".to_vec())).await }
    });
    advance_in_100ms_chunks(Duration::from_millis(10)).await;
    h.await.unwrap().unwrap();

    // A cut, recorded as an error.
    sim.apply_fault(Fault::Cut { dst });
    let _ = rec.request(dst, HttpRequest::get("/gone")).await;

    let cap = rec.captured();
    assert_eq!(cap.len(), 2);
    assert_eq!(cap[0].method, "POST");
    assert_eq!(cap[0].path, "/hi");
    assert_eq!(cap[0].req_body, b"body");
    assert!(matches!(
        cap[0].outcome,
        ExchangeOutcome::Response { status: 200, .. }
    ));
    assert!(matches!(cap[1].outcome, ExchangeOutcome::Error(_)));
}

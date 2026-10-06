//! `RecordingHttpNetwork` on the `layer-harness` `Recorder`: every exchange is
//! events on one typed channel, stamped from the recorder's sequencer, so HTTP
//! interleaves with every other channel of the run. The served side is
//! recorded by `serve()`; the client side by `request()`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http_net::{
    to_http_entries, ExchangeOutcome, Fault, HttpAnswer, HttpError, HttpNetworkEvent, HttpOutcome,
    HttpRequest, HttpResponse, HttpService, HttpTransport, RecordingHttpNetwork,
    SimulatedHttpNetwork, HTTP_TAG,
};
use layer_harness::{lane_key, NetworkTag, Recorder, TransportKind};
use sip_clock::testkit::advance_settled;
use sip_clock::Clock;

const CLIENT: &str = "10.0.0.5:5060";

fn addr(s: &str) -> std::net::SocketAddr {
    s.parse().unwrap()
}

fn recorder() -> Recorder {
    Recorder::with_clock(TransportKind::Fake, Clock::test_at(0))
}

struct Ok200;

#[async_trait]
impl HttpService for Ok200 {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        HttpResponse::ok(req.body).header("x-served", "yes")
    }
}

/// Never answers.
struct Withhold;

#[async_trait]
impl HttpService for Withhold {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        std::future::pending().await
    }
}

/// Closes the connection without a response.
struct Resets;

#[async_trait]
impl HttpService for Resets {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        HttpResponse::status(500)
    }

    async fn answer(&self, _req: HttpRequest) -> HttpAnswer {
        HttpAnswer::Abort
    }
}

async fn send(rec: &RecordingHttpNetwork, dst: std::net::SocketAddr, path: &str) {
    let h = tokio::spawn({
        let rec = rec.clone();
        let req = HttpRequest::post(path, b"{\"k\":1}".to_vec());
        async move { rec.request(dst, req).await }
    });
    advance_settled(Duration::from_millis(10)).await;
    let _ = h.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_client_exchange_rides_the_recorder_sequence_between_other_channels() {
    let recorder = recorder();
    let other = recorder.for_tag::<&'static str>("demo/Other");
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let dst = addr("10.0.0.1:8080");
    let _server = sim.serve(dst, Arc::new(Ok200)).await.unwrap();
    let rec = RecordingHttpNetwork::new(sim.clone(), &recorder, CLIENT);

    other.record("before");
    send(&rec, dst, "/call/new").await;
    other.record("after");

    let events = recorder.for_tag::<HttpNetworkEvent>(HTTP_TAG).snapshot();
    assert_eq!(events.len(), 2, "request and reply: {events:?}");
    let before = other.snapshot()[0].seq;
    let after = other.snapshot()[1].seq;
    assert!(before < events[0].seq && events[1].seq < after, "one sequence: {events:?}");
    match &events[0].event {
        HttpNetworkEvent::Sent { client, dst: to, request } => {
            assert_eq!(client, CLIENT);
            assert_eq!(*to, dst);
            assert_eq!(request.path, "/call/new");
        }
        other => panic!("expected the request first, got {other:?}"),
    }
    match &events[1].event {
        HttpNetworkEvent::Received { exchange, outcome: HttpOutcome::Response(resp) } => {
            assert_eq!(*exchange, events[0].seq, "the reply names its request");
            assert_eq!(resp.status, 200);
        }
        other => panic!("expected the reply, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn serve_records_the_served_side_paired_with_the_recording_client() {
    let recorder = recorder();
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let dst = addr("10.0.0.1:8080");
    let rec = RecordingHttpNetwork::new(sim.clone(), &recorder, CLIENT);
    let _server = rec.serve(dst, Arc::new(Ok200)).await.unwrap();

    send(&rec, dst, "/call/new").await;

    let entries = to_http_entries(&recorder.for_tag::<HttpNetworkEvent>(HTTP_TAG).snapshot());
    assert_eq!(entries.len(), 1, "one exchange, drawn once: {entries:?}");
    let e = &entries[0];
    assert!(e.served, "a recorded service saw it, so the served side is the row source");
    assert_eq!(e.requester.as_deref(), Some(CLIENT), "paired with the client's lane");
    assert_eq!(e.service, lane_key(dst));
    assert_eq!(e.request.path, "/call/new");
    match &e.outcome {
        Some(HttpOutcome::Response(resp)) => assert_eq!(resp.status, 200),
        other => panic!("expected a response, got {other:?}"),
    }
    assert!(e.reply_seq.is_some_and(|s| s > e.seq));
}

#[tokio::test(start_paused = true)]
async fn serve_registers_the_service_lane() {
    let recorder = recorder();
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let dst = addr("10.0.0.1:8080");
    let rec =
        RecordingHttpNetwork::new(sim.clone(), &recorder, CLIENT).with_service_name(dst, "bl");
    let _server = rec.serve(dst, Arc::new(Ok200)).await.unwrap();

    let lanes = recorder.snapshot().lanes;
    let lane = lanes.iter().find(|l| l.key == lane_key(dst)).expect("a lane for the service");
    assert_eq!(lane.network, NetworkTag::Service);
    assert_eq!(lane.names, vec!["bl".to_string()]);
}

#[tokio::test(start_paused = true)]
async fn a_request_from_an_unrecorded_client_is_served_without_a_requester() {
    let recorder = recorder();
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let dst = addr("10.0.0.1:8080");
    let rec = RecordingHttpNetwork::new(sim.clone(), &recorder, CLIENT);
    let _server = rec.serve(dst, Arc::new(Ok200)).await.unwrap();

    let h = tokio::spawn({
        let sim = sim.clone();
        async move { sim.request(dst, HttpRequest::post("/call/new", Vec::new())).await }
    });
    advance_settled(Duration::from_millis(10)).await;
    h.await.unwrap().unwrap();

    let entries = to_http_entries(&recorder.for_tag::<HttpNetworkEvent>(HTTP_TAG).snapshot());
    assert_eq!(entries.len(), 1);
    assert!(entries[0].served);
    assert_eq!(entries[0].requester, None);
    assert_eq!(entries[0].peer, None, "the simulated fabric carries no peer address");
    assert!(rec.exchanges().is_empty(), "the recording client made no exchange");
}

#[tokio::test(start_paused = true)]
async fn a_request_no_service_saw_is_drawn_from_the_client_side() {
    let recorder = recorder();
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let dst = addr("10.0.0.1:8080");
    let rec = RecordingHttpNetwork::new(sim.clone(), &recorder, CLIENT);
    let _server = rec.serve(dst, Arc::new(Ok200)).await.unwrap();
    sim.apply_fault(Fault::Cut { dst });

    let err = rec.request(dst, HttpRequest::get("/gone")).await.unwrap_err();
    assert!(matches!(err, HttpError::Connect(_)));

    let entries = to_http_entries(&recorder.for_tag::<HttpNetworkEvent>(HTTP_TAG).snapshot());
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert!(!e.served);
    assert_eq!(e.requester.as_deref(), Some(CLIENT));
    assert_eq!(e.service, lane_key(dst));
    assert!(matches!(e.outcome, Some(HttpOutcome::Error(_))), "{:?}", e.outcome);
}

#[tokio::test(start_paused = true)]
async fn a_withheld_answer_is_abandoned_on_both_sides_when_the_caller_gives_up() {
    let recorder = recorder();
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let dst = addr("10.0.0.1:8080");
    let rec = RecordingHttpNetwork::new(sim.clone(), &recorder, CLIENT);
    let _server = rec.serve(dst, Arc::new(Withhold)).await.unwrap();

    let h = tokio::spawn({
        let rec = rec.clone();
        async move {
            tokio::time::timeout(
                Duration::from_millis(150),
                rec.request(dst, HttpRequest::post("/call/new", b"asked".to_vec())),
            )
            .await
        }
    });
    advance_settled(Duration::from_millis(200)).await;
    assert!(h.await.unwrap().is_err(), "the caller's budget fires");

    let events = recorder.for_tag::<HttpNetworkEvent>(HTTP_TAG).snapshot();
    assert!(
        events.iter().any(|e| matches!(
            e.event,
            HttpNetworkEvent::Received { outcome: HttpOutcome::Abandoned, .. }
        )),
        "the client side ends abandoned: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e.event,
            HttpNetworkEvent::Answered { outcome: HttpOutcome::Abandoned, .. }
        )),
        "the served side ends abandoned: {events:?}"
    );
    let entries = to_http_entries(&events);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Some(HttpOutcome::Abandoned));

    let cap = rec.exchanges();
    assert_eq!(cap.len(), 1, "the abandoned request is in the client view");
    assert!(
        matches!(&cap[0].outcome, ExchangeOutcome::Error(d) if d.starts_with("timed out")),
        "{:?}",
        cap[0].outcome
    );
}

#[tokio::test(start_paused = true)]
async fn serve_forwards_answer_so_a_reset_stays_a_reset() {
    let recorder = recorder();
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let dst = addr("10.0.0.1:8080");
    let rec = RecordingHttpNetwork::new(sim.clone(), &recorder, CLIENT);
    let _server = rec.serve(dst, Arc::new(Resets)).await.unwrap();

    let h = tokio::spawn({
        let rec = rec.clone();
        async move { rec.request(dst, HttpRequest::post("/call/new", Vec::new())).await }
    });
    advance_settled(Duration::from_millis(10)).await;
    let err = h.await.unwrap().unwrap_err();
    assert!(matches!(err, HttpError::Io { .. }), "a reset reaches the client: {err:?}");

    let entries = to_http_entries(&recorder.for_tag::<HttpNetworkEvent>(HTTP_TAG).snapshot());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Some(HttpOutcome::Abort));
}

#[tokio::test(start_paused = true)]
async fn the_client_view_is_stamped_on_the_recorder_clock_in_request_order() {
    let clock = Clock::test_at(0);
    let recorder = Recorder::with_clock(TransportKind::Fake, clock.clone());
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let dst = addr("10.0.0.1:8080");
    let rec = RecordingHttpNetwork::new(sim.clone(), &recorder, CLIENT);
    let _server = rec.serve(dst, Arc::new(Ok200)).await.unwrap();
    sim.apply_fault(Fault::Delay { dst, ms: 500 });

    let sent_at = clock.now_ms();
    let h = tokio::spawn({
        let rec = rec.clone();
        async move { rec.request(dst, HttpRequest::get("/x")).await }
    });
    advance_settled(Duration::from_millis(2000)).await;
    h.await.unwrap().unwrap();
    send(&rec, dst, "/y").await;

    let cap = rec.exchanges();
    assert_eq!(cap.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["/x", "/y"]);
    assert!(cap[0].at_ms - sent_at < 500, "stamped at the request: {}", cap[0].at_ms);
    assert!(matches!(
        &cap[0].outcome,
        ExchangeOutcome::Response { status: 200, headers, .. }
            if headers.iter().any(|(k, v)| k == "x-served" && v == "yes")
    ));
}

/// On a real socket the served record carries the connection's remote address
/// and no client exchange: the requester is out of the recording.
#[tokio::test]
async fn a_request_served_on_a_real_socket_records_its_peer() {
    let recorder = Recorder::with_clock(TransportKind::Live, Clock::system());
    let real = Arc::new(http_net::RealHttpNetwork::new());
    let rec = RecordingHttpNetwork::new(real.clone(), &recorder, CLIENT);
    let server = rec.serve(addr("127.0.0.1:0"), Arc::new(Ok200)).await.unwrap();
    let dst = server.local_addr();

    let resp = real.request(dst, HttpRequest::post("/call/new", b"x".to_vec())).await.unwrap();
    assert_eq!(resp.status, 200);

    let entries = to_http_entries(&recorder.for_tag::<HttpNetworkEvent>(HTTP_TAG).snapshot());
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert!(e.served);
    assert_eq!(e.requester, None);
    assert_eq!(e.service, lane_key(dst), "the bound address, not the requested port 0");
    let peer = e.peer.expect("the peer address is recorded");
    assert!(peer.ip().is_loopback() && peer != dst, "{peer}");
    assert!(matches!(&e.outcome, Some(HttpOutcome::Response(r)) if r.status == 200));
}

/// Both sides recorded on one recorder over real sockets: the served handler
/// runs on the server's connection task, out of the client's future, and the
/// exchange is still drawn once, from the served side, paired to the client.
#[tokio::test]
async fn both_sides_recorded_on_real_sockets_draw_one_exchange() {
    let recorder = Recorder::with_clock(TransportKind::Live, Clock::system());
    let rec =
        RecordingHttpNetwork::new(Arc::new(http_net::RealHttpNetwork::new()), &recorder, CLIENT);
    let server = rec.serve(addr("127.0.0.1:0"), Arc::new(Ok200)).await.unwrap();
    let dst = server.local_addr();

    for body in [&b"one"[..], &b"two"[..]] {
        let resp = rec.request(dst, HttpRequest::post("/call/new", body.to_vec())).await.unwrap();
        assert_eq!(resp.status, 200);
    }

    let entries = to_http_entries(&recorder.for_tag::<HttpNetworkEvent>(HTTP_TAG).snapshot());
    assert_eq!(entries.len(), 2, "each exchange drawn once: {entries:?}");
    for (e, body) in entries.iter().zip([&b"one"[..], &b"two"[..]]) {
        assert!(e.served, "the served side is the row source");
        assert_eq!(e.requester.as_deref(), Some(CLIENT), "paired to the recording client");
        assert_eq!(e.request.body, body);
        assert!(e.peer.is_some());
    }
    assert_eq!(rec.exchanges().len(), 2, "the client view is unchanged");
}

/// The caller stops waiting while the reply is in transit: the service
/// answered, the client never got it. The drawn reply is the client's view.
#[tokio::test(start_paused = true)]
async fn a_caller_that_gives_up_during_the_reply_transit_is_drawn_without_the_reply() {
    let recorder = recorder();
    let sim = Arc::new(SimulatedHttpNetwork::new());
    let dst = addr("10.0.0.1:8080");
    let rec = RecordingHttpNetwork::new(sim.clone(), &recorder, CLIENT);
    let _server = rec.serve(dst, Arc::new(Ok200)).await.unwrap();
    // 100 ms each way: the service answers at 100 ms, the reply would land at
    // 200 ms, the caller gives up at 150 ms.
    sim.apply_fault(Fault::Delay { dst, ms: 100 });

    let h = tokio::spawn({
        let rec = rec.clone();
        async move {
            tokio::time::timeout(
                Duration::from_millis(150),
                rec.request(dst, HttpRequest::post("/call/new", Vec::new())),
            )
            .await
        }
    });
    advance_settled(Duration::from_millis(300)).await;
    assert!(h.await.unwrap().is_err(), "the caller gave up");

    let events = recorder.for_tag::<HttpNetworkEvent>(HTTP_TAG).snapshot();
    assert!(
        events.iter().any(|e| matches!(
            e.event,
            HttpNetworkEvent::Answered { outcome: HttpOutcome::Response(_), .. }
        )),
        "the service did answer: {events:?}"
    );
    let entries = to_http_entries(&events);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Some(HttpOutcome::Abandoned), "the client's view wins");
}

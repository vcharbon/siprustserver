//! `Harness::advance` runs the work in flight at every instant it crosses: a
//! request a detached task sends inside the advance is answered at its wire
//! instant, within its budget, and is not starved into a timeout by the jump.
//!
//! bob consults a service through a client task while ringing, the way a
//! SUT's service client answers its caller through a channel:
//!
//! ```text
//!   alice ──INVITE──▶ bob, alice ◀──180── bob
//!   bob ──consult──▶ client task ──POST /route──▶ routing   (60 ms transit)
//!                    client task ◀──200─────────── routing   (60 ms transit)
//!   bob ◀──answer── client task                               (budget 150 ms)
//!   …the test advances 1 s over the consult…
//!   alice ◀──200── bob, ACK, BYE/200
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http_net::{
    HttpRequest, HttpResponse, HttpService, HttpTransport, RecordingHttpNetwork,
    SimulatedHttpNetwork,
};
use scenario_harness::Harness;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

const SDP_OFFER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const SDP_ANSWER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

const BOB: &str = "127.0.0.1:5070";
/// One-way transit to the service.
const TRANSIT: Duration = Duration::from_millis(60);
/// The caller's budget for the answer: above the round trip.
const BUDGET: Duration = Duration::from_millis(150);

/// Answers every request 200.
struct Routing;

#[async_trait]
impl HttpService for Routing {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        HttpResponse::ok(br#"{"target":"bob"}"#.to_vec())
    }
}

/// Spawns a client task that owns `net` and a caller that consults it under
/// [`BUDGET`]; the receiver yields the caller's outcome and the virtual time it
/// took, `None` for a timeout.
fn consult(net: RecordingHttpNetwork, service: SocketAddr) -> oneshot::Receiver<Option<Duration>> {
    let (to_client, mut requests) = mpsc::channel::<oneshot::Sender<u16>>(1);
    tokio::spawn(async move {
        while let Some(reply) = requests.recv().await {
            let answer = net
                .request(service, HttpRequest::post("/route", br#"{"call":"c-1"}"#.to_vec()))
                .await
                .expect("the service answers");
            let _ = reply.send(answer.status);
        }
    });
    let (done, outcome) = oneshot::channel();
    tokio::spawn(async move {
        let started = Instant::now();
        let (reply, answer) = oneshot::channel();
        to_client.send(reply).await.expect("the client task runs");
        let answered = tokio::time::timeout(BUDGET, answer).await;
        let _ = done.send(match answered {
            Ok(Ok(200)) => Some(started.elapsed()),
            _ => None,
        });
    });
    outcome
}

#[tokio::test(start_paused = true)]
async fn a_consult_sent_inside_an_advance_lands_at_its_wire_instant() {
    let h = Harness::new("advance-settles");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", BOB).await;
    let service: SocketAddr = "10.9.0.1:8080".parse().unwrap();
    let net = RecordingHttpNetwork::new(
        Arc::new(SimulatedHttpNetwork::with_transit_delay(TRANSIT.as_millis() as u64)),
        &h.recorder(),
        BOB,
    )
    .with_service_name(service, "routing");
    let _server = net.serve(service, Arc::new(Routing)).await.unwrap();

    let mut call = alice.invite(&bob).with_sdp(SDP_OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let outcome = consult(net, service);
    h.advance(Duration::from_secs(1)).await;
    let took = outcome.await.expect("the caller reports");
    assert_eq!(
        took,
        Some(2 * TRANSIT),
        "the consult is answered one round trip after it left, inside its budget"
    );

    uas.respond(200, "OK").with_sdp(SDP_ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    assert!(report.passed(), "a compliant dialog: {:?}", report.extra_anomalies);
}

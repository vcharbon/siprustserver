//! HTTP exchanges in the scenario ladder: a scripted HTTP service recorded on
//! the harness's own `Recorder`, its exchanges drawn between the SIP messages
//! they happened between, its verdicts in the report — on the simulated fabric
//! under a paused clock and on real sockets under the real clock.
//!
//! The flow is a complete dialog; bob consults an HTTP service between
//! receiving the INVITE and ringing:
//!
//! ```text
//!   alice ──INVITE──▶ bob ──POST /route──▶ routing
//!                     bob ◀──200────────── routing
//!   alice ◀──180/200── bob, ACK, BYE/200
//! ```

use std::sync::Arc;

use http_net::scripted::{
    HttpBindings, HttpReifiedStep, HttpReply, HttpRequestMatch, HttpScript, HttpScriptHandle,
    ScriptedHttpService,
};
use http_net::{HttpRequest, HttpResponse, HttpTransport, RecordingHttpNetwork};
use layer_harness::{lane_key, NetworkTag};
use scenario_harness::report::{self, http::verdict_anomalies};
use scenario_harness::{Agent, Harness, RunReport};
use seq_report::{LaneKind, RowKind, SeqDoc};

const SDP_OFFER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const SDP_ANSWER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

const BOB: &str = "127.0.0.1:5070";

fn route_script() -> HttpScript {
    HttpScript::reified(
        HttpRequestMatch::post("/route").contains(r#""call":"${bind:call}""#),
        vec![HttpReifiedStep::new(
            HttpRequestMatch::post("/route"),
            HttpReply::respond(200, r#"{"target":"${bind:target}"}"#),
        )],
    )
}

fn bindings() -> HttpBindings {
    HttpBindings::new().bind("call", "c-1").bind("target", "bob")
}

/// The dialog of the module docs; `ask` is bob's consultation of the service.
async fn dialog<F, Fut>(alice: &Agent, bob: &Agent, ask: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = HttpResponse>,
{
    let mut call = alice.invite(bob).with_sdp(SDP_OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    let answer = ask().await;
    assert_eq!(answer.status, 200, "{}", String::from_utf8_lossy(&answer.body));
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(SDP_ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
}

/// The ladder labels in render order.
fn labels(doc: &SeqDoc) -> Vec<String> {
    let mut rows = doc.rows.clone();
    rows.sort_by_key(|r| r.seq);
    rows.iter().map(|r| r.label.split_whitespace().next().unwrap_or("").to_string()).collect()
}

fn fold_verdict(report: &mut RunReport, handle: &HttpScriptHandle) {
    let findings = handle.verdict().findings;
    report.extra_anomalies.extend(verdict_anomalies(&findings, &report.http_entries()));
}

#[tokio::test(start_paused = true)]
async fn the_exchange_sits_between_the_sip_messages_on_the_simulated_fabric() {
    let h = Harness::new("http-simulated");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", BOB).await;

    let svc = ScriptedHttpService::new();
    let handle = svc.add(route_script(), bindings()).unwrap();
    let service: std::net::SocketAddr = "10.9.0.1:8080".parse().unwrap();
    let net = RecordingHttpNetwork::new(
        Arc::new(http_net::SimulatedHttpNetwork::new()),
        &h.recorder(),
        BOB,
    )
    .with_service_name(service, "routing");
    let _server = net.serve(service, Arc::new(svc.clone())).await.unwrap();

    dialog(&alice, &bob, || async {
        net.request(service, HttpRequest::post("/route", br#"{"call":"c-1"}"#.to_vec()))
            .await
            .unwrap()
    })
    .await;

    let mut report = h.finish().await;
    fold_verdict(&mut report, &handle);
    let doc = report::seq_doc(&report);

    assert_eq!(
        labels(&doc),
        ["INVITE", "POST", "200", "180", "200", "ACK", "BYE", "200"],
        "the request and its reply sit between the INVITE and the 180"
    );
    let http: Vec<_> = doc.rows.iter().filter(|r| matches!(r.kind, RowKind::Http { .. })).collect();
    assert_eq!(http.len(), 2);
    let svc_lane = doc.lanes.iter().find(|l| l.id == lane_key(service)).expect("service lane");
    assert_eq!(svc_lane.kind, LaneKind::Service);
    assert!(svc_lane.label.starts_with("routing"), "{}", svc_lane.label);
    assert_eq!(http[0].from, BOB, "the request leaves bob's lane");
    assert_eq!(http[0].to.as_deref(), Some(svc_lane.id.as_str()));
    assert!(http[0].detail.as_deref().unwrap_or("").contains("\"call\": \"c-1\""));
    assert!(doc.passed, "a served script is green: {:?}", doc.anomalies);
    assert!(report.scenario().lanes.iter().any(|l| l.network == NetworkTag::Service));
}

#[tokio::test(start_paused = true)]
async fn an_unmatched_request_is_a_gating_finding_linked_to_its_row() {
    let h = Harness::new("http-unmatched");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", BOB).await;

    let svc = ScriptedHttpService::new();
    let handle = svc.add(route_script(), bindings()).unwrap();
    let service: std::net::SocketAddr = "10.9.0.1:8080".parse().unwrap();
    let net = RecordingHttpNetwork::new(
        Arc::new(http_net::SimulatedHttpNetwork::new()),
        &h.recorder(),
        BOB,
    );
    let _server = net.serve(service, Arc::new(svc.clone())).await.unwrap();

    // The expected consultation, then one the script does not state.
    dialog(&alice, &bob, || async {
        net.request(service, HttpRequest::post("/route", br#"{"call":"c-1"}"#.to_vec()))
            .await
            .unwrap()
    })
    .await;
    let stray = net.request(service, HttpRequest::post("/stray", b"{}".to_vec())).await.unwrap();
    assert_eq!(stray.status, 500);

    let mut report = h.finish().await;
    fold_verdict(&mut report, &handle);
    // The service-level finding is attributed to no instance: fold it too.
    report.extra_anomalies.extend(verdict_anomalies(&svc.findings(), &report.http_entries()));
    let doc = report::seq_doc(&report);

    let stray_row = doc
        .rows
        .iter()
        .find(|r| r.label.starts_with("POST /stray"))
        .expect("the stray request is drawn");
    let finding = doc
        .anomalies
        .iter()
        .find(|a| a.check == "http.unmatched")
        .unwrap_or_else(|| panic!("an unmatched finding: {:?}", doc.anomalies));
    assert_eq!(finding.advisory, Some(false), "gating");
    assert!(!finding.rule_sourced, "not an RFC rule");
    assert_eq!(finding.row_seqs, vec![stray_row.seq], "linked to the request row");
    assert!(!doc.passed, "a gating HTTP finding fails the doc");
    assert!(!report.passed(), "and the run");
}

#[tokio::test(start_paused = true)]
async fn the_written_artifacts_carry_the_service_view() {
    let h = Harness::new("http-artifacts");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", BOB).await;
    let svc = ScriptedHttpService::new();
    svc.add(route_script(), bindings()).unwrap();
    let service: std::net::SocketAddr = "10.9.0.1:8080".parse().unwrap();
    let net = RecordingHttpNetwork::new(
        Arc::new(http_net::SimulatedHttpNetwork::new()),
        &h.recorder(),
        BOB,
    )
    .with_service_name(service, "routing");
    let _server = net.serve(service, Arc::new(svc.clone())).await.unwrap();
    dialog(&alice, &bob, || async {
        net.request(service, HttpRequest::post("/route", br#"{"call":"c-1"}"#.to_vec()))
            .await
            .unwrap()
    })
    .await;
    let report = h.finish().await;

    let out = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("http-artifacts");
    let _ = std::fs::remove_dir_all(&out);
    report::write_all(&report, &out).unwrap();
    let global = std::fs::read_to_string(out.join("http-artifacts.global.txt")).unwrap();
    assert!(global.contains("[HTTP] bob (127.0.0.1:5070) -> routing (10.9.0.1:8080)  POST /route"));
    let view = std::fs::read_to_string(out.join("service/routing.txt")).unwrap();
    assert!(view.contains("POST /route"), "{view}");
    assert!(view.contains(r#""target":"bob""#), "the reply body as sent: {view}");
    let html = std::fs::read_to_string(out.join("http-artifacts.html")).unwrap();
    assert!(html.contains("seq-http"), "the html draws the HTTP plane");
    let svg = std::fs::read_to_string(out.join("http-artifacts.svg")).unwrap();
    assert_eq!(svg, seq_report::render_svg(&report::seq_doc(&report)), "the svg is the doc's");
    assert!(svg.contains("seq-http"), "the svg draws the HTTP plane");

    // The service view counts time from the run's start, as the global view.
    let stamp = |text: &str| {
        let at = text.find("POST /route").unwrap();
        let open = text[..at].rfind("[T+").unwrap();
        text[open..open + text[open..].find(']').unwrap() + 1].to_string()
    };
    assert_eq!(stamp(&view), stamp(&global), "one timeline:\n{view}\n{global}");
    assert_ne!(stamp(&view), "[T+0.000s]", "the request came after the INVITE");
}

#[tokio::test(start_paused = true)]
async fn a_service_nobody_called_draws_no_column() {
    let h = Harness::new("http-unused");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", BOB).await;
    let service: std::net::SocketAddr = "10.9.0.1:8080".parse().unwrap();
    let net = RecordingHttpNetwork::new(
        Arc::new(http_net::SimulatedHttpNetwork::new()),
        &h.recorder(),
        BOB,
    )
    .with_service_name(service, "routing");
    let _server = net.serve(service, Arc::new(ScriptedHttpService::new())).await.unwrap();
    dialog(&alice, &bob, || async { HttpResponse::ok(Vec::new()) }).await;
    let report = h.finish().await;

    let doc = report::seq_doc(&report);
    assert!(doc.lanes.iter().all(|l| l.kind != LaneKind::Service), "{:?}", doc.lanes);
    let out = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("http-unused");
    let _ = std::fs::remove_dir_all(&out);
    report::write_all(&report, &out).unwrap();
    let svg = std::fs::read_to_string(out.join("http-unused.svg")).unwrap();
    assert!(!svg.contains("routing"), "no empty service column: {svg}");
}

/// The service on a real socket, called by a client outside the recording (as
/// a separate SUT process would): the served side alone draws the exchange,
/// from a lane named after the peer's address.
#[tokio::test]
async fn the_served_side_alone_draws_the_exchange_on_real_sockets() {
    let h = Harness::new("http-real");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", BOB).await;

    let svc = ScriptedHttpService::new();
    let handle = svc.add(route_script(), bindings()).unwrap();
    let net =
        RecordingHttpNetwork::new(Arc::new(http_net::RealHttpNetwork::new()), &h.recorder(), BOB);
    let server = net.serve("127.0.0.1:0".parse().unwrap(), Arc::new(svc.clone())).await.unwrap();
    let service = server.local_addr();
    let outside = http_net::RealHttpNetwork::new();

    dialog(&alice, &bob, || async {
        outside
            .request(service, HttpRequest::post("/route", br#"{"call":"c-1"}"#.to_vec()))
            .await
            .unwrap()
    })
    .await;

    let mut report = h.finish().await;
    fold_verdict(&mut report, &handle);
    let doc = report::seq_doc(&report);

    assert_eq!(labels(&doc), ["INVITE", "POST", "200", "180", "200", "ACK", "BYE", "200"]);
    let request = doc.rows.iter().find(|r| r.label.starts_with("POST")).unwrap();
    assert_eq!(request.to.as_deref(), Some(lane_key(service).as_str()));
    let from = doc.lanes.iter().find(|l| l.id == request.from).expect("the requester's lane");
    assert!(from.label.contains("127.0.0.1"), "named after the peer: {}", from.label);
    assert_ne!(from.id, BOB, "no recording client, so no pairing with bob's lane");
    assert!(doc.passed, "{:?}", doc.anomalies);
}

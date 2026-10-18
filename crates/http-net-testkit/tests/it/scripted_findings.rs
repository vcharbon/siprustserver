//! Reading a shared `ScriptedHttpService`'s findings a few instances at a
//! time: the findings that name the given instances and their `Unserved`, and
//! the findings that name no instance, each without the rest of the service's.

use std::sync::Arc;

use http_net::{HttpRequest, HttpResponse, HttpTransport};
use http_net_testkit::scripted::{
    HttpBindings, HttpFindingKind, HttpReifiedStep, HttpReply, HttpRequestMatch, HttpScript,
    ScriptedHttpService,
};
use http_net_testkit::SimulatedHttpNetwork;

/// `POST /start` opens (keyed on the cell) and hands a token; `POST /next`
/// returns it and completes.
fn two_step() -> HttpScript {
    HttpScript::reified(
        HttpRequestMatch::post("/start").contains(r#""cell":"${bind:cell}""#),
        vec![
            HttpReifiedStep::new(
                HttpRequestMatch::post("/start"),
                HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
            ),
            HttpReifiedStep::new(
                HttpRequestMatch::post("/next").contains(r#""cell":"${bind:cell}""#),
                HttpReply::respond(200, "{}"),
            ),
        ],
    )
}

fn ctx(resp: &HttpResponse) -> String {
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    v["ctx"].as_str().unwrap().to_string()
}

#[tokio::test(start_paused = true)]
async fn a_few_instances_read_their_own_findings_and_the_unattributed_ones_apart() {
    let svc = ScriptedHttpService::new();
    let handles: Vec<_> = (0..50)
        .map(|n| svc.add(two_step(), HttpBindings::new().bind("cell", format!("c{n}"))).unwrap())
        .collect();
    let net = SimulatedHttpNetwork::new();
    let dst = "10.0.0.7:8080".parse().unwrap();
    let _server = net.serve(dst, Arc::new(svc.clone())).await.unwrap();
    let post =
        |path: &str, body: String| net.request(dst, HttpRequest::post(path, body.into_bytes()));

    // c3 opens, then follows with another cell's body: a finding naming it.
    let c3 = ctx(&post("/start", r#"{"cell":"c3"}"#.into()).await.unwrap());
    let refused = post("/next", format!(r#"{{"cell":"c4","ctx":"{c3}"}}"#)).await.unwrap();
    assert_eq!(refused.status, 500);
    // c7 is served whole.
    let c7 = ctx(&post("/start", r#"{"cell":"c7"}"#.into()).await.unwrap());
    let done = post("/next", format!(r#"{{"cell":"c7","ctx":"{c7}"}}"#)).await.unwrap();
    assert_eq!(done.status, 200);
    // A request no script opens names no instance.
    assert_eq!(post("/start", r#"{"cell":"zz"}"#.into()).await.unwrap().status, 500);

    let (three, seven) = (handles[3].instance(), handles[7].instance());
    let mine = svc.findings_of(&[three, seven]);
    let kinds: Vec<_> = mine.iter().map(|f| (f.kind, f.instances.clone())).collect();
    assert_eq!(
        kinds,
        [(HttpFindingKind::Unmatched, vec![three]), (HttpFindingKind::Unserved, vec![three])],
        "c3's refusal and its unserved step, nothing of c7 nor of the 48 others"
    );
    assert_eq!(mine, handles[3].verdict().findings, "the same findings its verdict states");
    assert!(svc.findings_of(&[seven]).is_empty());

    let unattributed = svc.unattributed_findings();
    assert_eq!(unattributed.len(), 1, "{unattributed:?}");
    assert_eq!(unattributed[0].kind, HttpFindingKind::Unmatched);
    assert!(unattributed[0].body.contains("zz"));

    // The service-level read still states every finding.
    assert_eq!(svc.findings().len(), 2 + 49);
}

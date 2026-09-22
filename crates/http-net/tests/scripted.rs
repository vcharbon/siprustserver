//! `ScriptedHttpService` on the simulated fabric under a paused clock: the
//! program, the continuation token, the faults and the hard-failure rule.
//!
//! The client in these tests plays the peer's contract: it returns the token a
//! reply carried (the `ctx` field) byte-verbatim in its next request body.

use std::sync::Arc;
use std::time::Duration;

use http_net::scripted::{
    HttpBindings, HttpFindingKind, HttpReifiedStep, HttpReply, HttpRequestMatch, HttpScript,
    HttpScriptError, HttpState, HttpUnmatched, ScriptedHttpService,
};
use http_net::{
    HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpTransport, SimulatedHttpNetwork,
};

fn dst() -> std::net::SocketAddr {
    "10.0.0.7:8080".parse().unwrap()
}

async fn serve(svc: &ScriptedHttpService) -> (SimulatedHttpNetwork, Box<dyn HttpServerHandle>) {
    let net = SimulatedHttpNetwork::new();
    let handle = net.serve(dst(), Arc::new(svc.clone())).await.unwrap();
    (net, handle)
}

async fn post(net: &SimulatedHttpNetwork, path: &str, body: &str) -> HttpResponse {
    net.request(dst(), HttpRequest::post(path, body.as_bytes().to_vec())).await.unwrap()
}

/// The `ctx` field of a JSON reply: the token the peer must return.
fn ctx(resp: &HttpResponse) -> String {
    let v: serde_json::Value = serde_json::from_slice(&resp.body)
        .unwrap_or_else(|e| panic!("reply is not JSON ({e}): {}", body(resp)));
    v["ctx"].as_str().unwrap_or_else(|| panic!("no ctx in {}", body(resp))).to_string()
}

fn body(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn cell(name: &str) -> HttpBindings {
    HttpBindings::new().bind("cell", name)
}

/// `POST /start` opens (keyed on the cell), `POST /next` returns the token and
/// completes.
fn two_step() -> HttpScript {
    HttpScript::reified(
        HttpRequestMatch::post("/start").contains(r#""cell":"${bind:cell}""#),
        vec![
            HttpReifiedStep::new(
                HttpRequestMatch::post("/start"),
                HttpReply::respond(
                    200,
                    r#"{"ctx":"${continuation}","step":1,"cell":"${bind:cell}"}"#,
                ),
            ),
            HttpReifiedStep::new(
                HttpRequestMatch::post("/next").contains(r#""cell":"${bind:cell}""#),
                HttpReply::respond(200, r#"{"step":2,"cell":"${bind:cell}"}"#),
            ),
        ],
    )
}

#[tokio::test(start_paused = true)]
async fn a_two_step_script_is_served_under_a_paused_clock() {
    let svc = ScriptedHttpService::new();
    let script = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let first = post(&net, "/start", r#"{"cell":"c1"}"#).await;
    assert_eq!(first.status, 200, "{}", body(&first));
    let token = ctx(&first);
    assert!(!token.is_empty());

    let second = post(&net, "/next", &format!(r#"{{"cell":"c1","ctx":"{token}"}}"#)).await;
    assert_eq!(second.status, 200, "{}", body(&second));
    assert_eq!(body(&second), r#"{"step":2,"cell":"c1"}"#);

    let verdict = script.verdict();
    assert!(verdict.opened && verdict.complete, "{verdict:?}");
    assert!(verdict.is_green(), "{verdict:?}");
    assert!(svc.findings().is_empty(), "{:?}", svc.findings());
}

#[tokio::test(start_paused = true)]
async fn a_retransmitted_tokened_request_is_answered_identically() {
    let svc = ScriptedHttpService::new();
    let steps = vec![
        HttpReifiedStep::new(
            HttpRequestMatch::post("/start"),
            HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
        ),
        HttpReifiedStep::new(
            HttpRequestMatch::post("/next"),
            HttpReply::respond(200, r#"{"ctx":"${continuation}","step":2}"#).header("x-step", "2"),
        ),
        HttpReifiedStep::new(HttpRequestMatch::post("/last"), HttpReply::respond(204, "")),
    ];
    let script = svc
        .add(HttpScript::reified(HttpRequestMatch::post("/start"), steps), HttpBindings::new())
        .unwrap();
    let (net, _h) = serve(&svc).await;

    let t1 = ctx(&post(&net, "/start", "{}").await);
    let next = format!(r#"{{"ctx":"{t1}"}}"#);
    let a = post(&net, "/next", &next).await;
    let b = post(&net, "/next", &next).await;
    assert_eq!(a, b, "the same token and request get the same reply, token included");

    let last = post(&net, "/last", &format!(r#"{{"ctx":"{}"}}"#, ctx(&a))).await;
    assert_eq!(last.status, 204);
    let verdict = script.verdict();
    assert!(verdict.complete && verdict.is_green(), "{verdict:?}");
}

#[tokio::test(start_paused = true)]
async fn an_unmatched_opening_request_is_answered_500_and_recorded() {
    let svc = ScriptedHttpService::new();
    let script = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let resp = post(&net, "/elsewhere", r#"{"cell":"c1"}"#).await;
    assert_eq!(resp.status, 500);
    let other_cell = post(&net, "/start", r#"{"cell":"c2"}"#).await;
    assert_eq!(other_cell.status, 500, "a fragment the open states is missing");

    let findings = svc.findings();
    let unmatched: Vec<_> =
        findings.iter().filter(|f| f.kind == HttpFindingKind::Unmatched).collect();
    assert_eq!(unmatched.len(), 2, "{findings:?}");
    assert!(unmatched.iter().all(|f| f.instances.is_empty() && !f.is_advisory()));
    assert_eq!(unmatched[0].path, "/elsewhere");
    assert!(!script.verdict().opened);
}

#[tokio::test(start_paused = true)]
async fn a_wrong_request_at_the_token_position_is_unmatched() {
    let svc = ScriptedHttpService::new();
    let script = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let token = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    let wrong_path = post(&net, "/other", &format!(r#"{{"cell":"c1","ctx":"{token}"}}"#)).await;
    assert_eq!(wrong_path.status, 500);
    let wrong_body = post(&net, "/next", &format!(r#"{{"cell":"c9","ctx":"{token}"}}"#)).await;
    assert_eq!(wrong_body.status, 500);
    let diagnostic = body(&wrong_body);
    assert!(diagnostic.contains(r#""cell":"c1""#), "names what was expected: {diagnostic}");

    let verdict = script.verdict();
    let unmatched: Vec<_> =
        verdict.findings.iter().filter(|f| f.kind == HttpFindingKind::Unmatched).collect();
    assert_eq!(unmatched.len(), 2, "{verdict:?}");
    assert!(unmatched.iter().all(|f| f.instances == vec![script.instance()]));
    assert!(!verdict.complete, "a refused request does not advance the script");
    assert!(!verdict.is_green());
}

#[tokio::test(start_paused = true)]
async fn a_request_past_the_last_step_is_unmatched() {
    let svc = ScriptedHttpService::new();
    let one = HttpScript::reified(
        HttpRequestMatch::post("/start"),
        vec![HttpReifiedStep::new(
            HttpRequestMatch::post("/start"),
            HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
        )],
    );
    let script = svc.add(one, HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    let token = ctx(&post(&net, "/start", "{}").await);
    let past = post(&net, "/start", &format!(r#"{{"ctx":"{token}"}}"#)).await;
    assert_eq!(past.status, 500);

    let verdict = script.verdict();
    assert!(verdict.complete, "{verdict:?}");
    assert_eq!(verdict.findings.len(), 1, "{verdict:?}");
    assert_eq!(verdict.findings[0].kind, HttpFindingKind::Unmatched);
    assert!(!verdict.is_green(), "the SUT made a request the script ends before");
}

#[tokio::test(start_paused = true)]
async fn an_instance_short_of_its_last_step_is_unserved_at_run_end() {
    let svc = ScriptedHttpService::new();
    let script = svc.add(two_step(), cell("c1")).unwrap();
    let never = svc.add(two_step(), cell("c2")).unwrap();
    let (net, _h) = serve(&svc).await;

    assert_eq!(post(&net, "/start", r#"{"cell":"c1"}"#).await.status, 200);

    let verdict = script.verdict();
    assert!(verdict.opened && !verdict.complete, "{verdict:?}");
    assert_eq!(verdict.findings.len(), 1);
    assert_eq!(verdict.findings[0].kind, HttpFindingKind::Unserved);
    assert!(!verdict.is_green());
    let never = never.verdict();
    assert!(!never.opened && !never.is_green(), "{never:?}");

    let unserved: Vec<_> = svc
        .findings()
        .into_iter()
        .filter(|f| f.kind == HttpFindingKind::Unserved)
        .flat_map(|f| f.instances)
        .collect();
    assert_eq!(unserved.len(), 2, "both instances are unserved");
}

#[tokio::test(start_paused = true)]
async fn a_token_minted_by_another_service_is_an_advisory() {
    let earlier = ScriptedHttpService::with_nonce(1);
    earlier.add(two_step(), cell("c1")).unwrap();
    let (earlier_net, _e) = serve(&earlier).await;
    let stale = ctx(&post(&earlier_net, "/start", r#"{"cell":"c1"}"#).await);

    let svc = ScriptedHttpService::with_nonce(2);
    let script = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let resp = post(&net, "/next", &format!(r#"{{"cell":"c1","ctx":"{stale}"}}"#)).await;
    assert_eq!(resp.status, 500);
    let findings = svc.findings();
    let foreign: Vec<_> =
        findings.iter().filter(|f| f.kind == HttpFindingKind::ForeignToken).collect();
    assert_eq!(foreign.len(), 1, "{findings:?}");
    assert!(foreign[0].is_advisory());
    assert!(foreign[0].instances.is_empty(), "attributed to no instance");
    assert!(!script.verdict().opened, "a straggler opens nothing");
}

#[tokio::test(start_paused = true)]
async fn a_repeated_opening_request_consumes_the_next_identical_script() {
    let svc = ScriptedHttpService::new();
    let first = svc.add(two_step(), cell("c1")).unwrap();
    let second = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let a = post(&net, "/start", r#"{"cell":"c1"}"#).await;
    let b = post(&net, "/start", r#"{"cell":"c1"}"#).await;
    assert_eq!((a.status, b.status), (200, 200));
    assert_ne!(ctx(&a), ctx(&b), "each opening binds its own instance");
    assert!(first.verdict().opened && second.verdict().opened, "FIFO, one consumed each");

    let third = post(&net, "/start", r#"{"cell":"c1"}"#).await;
    assert_eq!(third.status, 500, "POST is not idempotent: a third opening is unmatched");
}

fn opening(fragments: &[&str], reply: &str) -> HttpScript {
    let mut open = HttpRequestMatch::post("/start");
    for f in fragments {
        open = open.contains(*f);
    }
    HttpScript::reified(
        open,
        vec![HttpReifiedStep::new(
            HttpRequestMatch::post("/start"),
            HttpReply::respond(200, reply),
        )],
    )
}

#[tokio::test(start_paused = true)]
async fn the_open_with_the_proper_superset_of_fragments_wins() {
    let svc = ScriptedHttpService::new();
    let broad = svc.add(opening(&[r#""a":1"#], "broad"), HttpBindings::new()).unwrap();
    let narrow =
        svc.add(opening(&[r#""a":1"#, r#""b":2"#], "narrow"), HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    assert_eq!(body(&post(&net, "/start", r#"{"a":1,"b":2}"#).await), "narrow");
    assert_eq!(body(&post(&net, "/start", r#"{"a":1}"#).await), "broad");
    assert!(broad.verdict().is_green() && narrow.verdict().is_green());
}

#[tokio::test(start_paused = true)]
async fn incomparable_overlapping_opens_are_ambiguous() {
    let svc = ScriptedHttpService::new();
    let a = svc.add(opening(&[r#""a":1"#], "a"), HttpBindings::new()).unwrap();
    let b = svc.add(opening(&[r#""b":2"#], "b"), HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    let resp = post(&net, "/start", r#"{"a":1,"b":2}"#).await;
    assert_eq!(resp.status, 500);
    let findings = svc.findings();
    let ambiguous: Vec<_> =
        findings.iter().filter(|f| f.kind == HttpFindingKind::Ambiguous).collect();
    assert_eq!(ambiguous.len(), 1, "{findings:?}");
    assert_eq!(ambiguous[0].instances, vec![a.instance(), b.instance()]);
    assert!(!a.verdict().opened && !b.verdict().opened, "an ambiguity opens nothing");
}

#[tokio::test(start_paused = true)]
async fn two_valid_tokens_in_one_body_are_ambiguous() {
    let svc = ScriptedHttpService::new();
    svc.add(two_step(), cell("c1")).unwrap();
    svc.add(two_step(), cell("c2")).unwrap();
    let (net, _h) = serve(&svc).await;
    let t1 = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    let t2 = ctx(&post(&net, "/start", r#"{"cell":"c2"}"#).await);

    let resp = post(&net, "/next", &format!(r#"{{"cell":"c1","ctx":"{t1}","old":"{t2}"}}"#)).await;
    assert_eq!(resp.status, 500);
    assert!(svc.findings().iter().any(|f| f.kind == HttpFindingKind::Ambiguous));
}

#[tokio::test(start_paused = true)]
async fn a_token_altered_in_transit_is_user_data() {
    let svc = ScriptedHttpService::new();
    let first = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;
    let token = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);

    // One character of the middle changed: the checksum fails, so the text is
    // the peer's data and the request is an opening request.
    let mut altered: Vec<char> = token.chars().collect();
    let mid = altered.len() / 2;
    altered[mid] = if altered[mid] == 'A' { 'B' } else { 'A' };
    let altered: String = altered.into_iter().collect();
    let second = svc.add(two_step(), cell("c1")).unwrap();
    let resp = post(&net, "/start", &format!(r#"{{"cell":"c1","ctx":"{altered}"}}"#)).await;
    assert_eq!(resp.status, 200, "{}", body(&resp));
    assert!(second.verdict().opened, "the altered token opened the next instance");
    assert!(!first.verdict().complete);
}

#[tokio::test(start_paused = true)]
async fn silence_lets_the_callers_timeout_fire_and_serves_the_step() {
    let svc = ScriptedHttpService::new();
    let silent = HttpScript::reified(
        HttpRequestMatch::post("/start"),
        vec![HttpReifiedStep::new(HttpRequestMatch::post("/start"), HttpReply::Silence)],
    );
    let script = svc.add(silent, HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    let outcome = tokio::time::timeout(
        Duration::from_millis(500),
        net.request(dst(), HttpRequest::post("/start", b"{}".to_vec())),
    )
    .await;
    assert!(outcome.is_err(), "no answer: the caller's own budget fires");
    let verdict = script.verdict();
    assert!(verdict.complete && verdict.is_green(), "served at arrival: {verdict:?}");
}

#[tokio::test(start_paused = true)]
async fn late_answers_after_the_stated_delay_plus_transit() {
    let svc = ScriptedHttpService::new();
    let late = HttpScript::reified(
        HttpRequestMatch::post("/start"),
        vec![HttpReifiedStep::new(
            HttpRequestMatch::post("/start"),
            HttpReply::respond(200, "late").late(300),
        )],
    );
    let script = svc.add(late, HttpBindings::new()).unwrap();
    let net = SimulatedHttpNetwork::with_transit_delay(5);
    let _h = net.serve(dst(), Arc::new(svc.clone())).await.unwrap();

    let t0 = tokio::time::Instant::now();
    let resp = net.request(dst(), HttpRequest::post("/start", b"{}".to_vec())).await.unwrap();
    assert_eq!(body(&resp), "late");
    assert_eq!(t0.elapsed(), Duration::from_millis(310), "ms + 2 x transit");
    assert!(script.verdict().is_green());
}

#[tokio::test(start_paused = true)]
async fn reset_is_a_connection_reset_on_the_simulated_fabric() {
    let svc = ScriptedHttpService::new();
    let reset = HttpScript::reified(
        HttpRequestMatch::post("/start"),
        vec![HttpReifiedStep::new(HttpRequestMatch::post("/start"), HttpReply::Reset)],
    );
    let script = svc.add(reset, HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    let err = net.request(dst(), HttpRequest::post("/start", b"{}".to_vec())).await.unwrap_err();
    match err {
        HttpError::Io { addr, reason } => {
            assert_eq!(addr, dst());
            assert!(reason.contains("reset"), "{reason}");
        }
        other => panic!("expected a reset, got {other:?}"),
    }
    assert!(script.verdict().is_green());
}

#[tokio::test(start_paused = true)]
async fn a_stated_503_is_a_response_not_a_finding() {
    let svc = ScriptedHttpService::new();
    let busy = HttpScript::reified(
        HttpRequestMatch::post("/start"),
        vec![HttpReifiedStep::new(
            HttpRequestMatch::post("/start"),
            HttpReply::respond(503, "").header("retry-after", "1"),
        )],
    );
    let script = svc.add(busy, HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    let resp = post(&net, "/start", "{}").await;
    assert_eq!(resp.status, 503);
    assert!(resp.headers.iter().any(|(k, v)| k == "retry-after" && v == "1"));
    assert!(script.verdict().is_green() && svc.findings().is_empty());
}

#[tokio::test(start_paused = true)]
async fn captures_travel_in_the_token_to_later_replies() {
    let svc = ScriptedHttpService::new();
    let echo = HttpScript::reified(
        HttpRequestMatch::post("/start").contains(r#""cell":"${bind:cell}""#),
        vec![
            HttpReifiedStep::new(
                HttpRequestMatch::post("/start")
                    .contains(r#""id":"${capture:id}""#)
                    .contains(r#""n":${capture:n}"#),
                HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
            ),
            HttpReifiedStep::new(
                HttpRequestMatch::post("/next").contains(r#""id":"${capture:id}""#),
                HttpReply::respond(200, r#"{"id":"${capture:id}","n":${capture:n}}"#),
            ),
        ],
    );
    let script = svc.add(echo, cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    // Pretty-printed by the peer: the compact fragments still match.
    let opening = "{\n  \"cell\": \"c1\",\n  \"id\": \"a\\\"b\",\n  \"n\": 42\n}";
    let token = ctx(&post(&net, "/start", opening).await);
    let resp = post(&net, "/next", &format!(r#"{{"id":"a\"b","ctx":"{token}"}}"#)).await;
    assert_eq!(resp.status, 200, "{}", body(&resp));
    assert_eq!(body(&resp), r#"{"id":"a\"b","n":42}"#);
    assert!(script.verdict().is_green());
}

#[tokio::test(start_paused = true)]
async fn a_capture_is_a_back_reference_once_taken() {
    let svc = ScriptedHttpService::new();
    let echo = HttpScript::reified(
        HttpRequestMatch::post("/start"),
        vec![
            HttpReifiedStep::new(
                HttpRequestMatch::post("/start").contains(r#""id":"${capture:id}""#),
                HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
            ),
            HttpReifiedStep::new(
                HttpRequestMatch::post("/next").contains(r#""id":"${capture:id}""#),
                HttpReply::respond(200, "ok"),
            ),
        ],
    );
    let script = svc.add(echo, HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    let token = ctx(&post(&net, "/start", r#"{"id":"x1"}"#).await);
    let other = post(&net, "/next", &format!(r#"{{"id":"x2","ctx":"{token}"}}"#)).await;
    assert_eq!(other.status, 500, "the captured id must come back unchanged");
    let same = post(&net, "/next", &format!(r#"{{"id":"x1","ctx":"{token}"}}"#)).await;
    assert_eq!(same.status, 200);
    assert!(script.verdict().complete);
}

#[tokio::test(start_paused = true)]
async fn a_code_step_threads_its_state_through_the_token() {
    let svc = ScriptedHttpService::new();
    let counter = HttpScript::code(
        HttpRequestMatch::post("/start").contains(r#""cell":"${bind:cell}""#),
        |req, bindings, state| {
            let cell = bindings.get("cell").unwrap_or_default();
            match state {
                None => Ok((
                    HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
                    Some(HttpState(serde_json::json!({ "count": 1 }))),
                )),
                Some(HttpState(v)) if req.path == "/next" => {
                    let count = v["count"].as_u64().unwrap_or(0) + 1;
                    Ok((
                        HttpReply::respond(200, format!(r#"{{"count":{count},"cell":"{cell}"}}"#)),
                        None,
                    ))
                }
                Some(_) => Err(HttpUnmatched::new(format!("expected /next, got {}", req.path))),
            }
        },
    );
    let script = svc.add(counter, cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let token = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    let refused = post(&net, "/wrong", &format!(r#"{{"ctx":"{token}"}}"#)).await;
    assert_eq!(refused.status, 500);
    assert!(body(&refused).contains("expected /next"), "{}", body(&refused));
    assert!(!script.verdict().complete);

    let done = post(&net, "/next", &format!(r#"{{"ctx":"{token}"}}"#)).await;
    assert_eq!(body(&done), r#"{"count":2,"cell":"c1"}"#);
    let verdict = script.verdict();
    assert!(verdict.complete, "{verdict:?}");
    assert_eq!(verdict.findings.len(), 1, "the refused request stays recorded");
    assert_eq!(verdict.findings[0].kind, HttpFindingKind::Unmatched);
}

#[tokio::test(start_paused = true)]
async fn concurrent_instances_are_told_apart_by_their_tokens() {
    let svc = ScriptedHttpService::new();
    let c1 = svc.add(two_step(), cell("c1")).unwrap();
    let c2 = svc.add(two_step(), cell("c2")).unwrap();
    let (net, _h) = serve(&svc).await;

    let (a, b) = tokio::join!(
        post(&net, "/start", r#"{"cell":"c2"}"#),
        post(&net, "/start", r#"{"cell":"c1"}"#)
    );
    let (tb, ta) = (ctx(&a), ctx(&b));
    let (next2, next1) =
        (format!(r#"{{"cell":"c2","ctx":"{tb}"}}"#), format!(r#"{{"cell":"c1","ctx":"{ta}"}}"#));
    let (r2, r1) = tokio::join!(post(&net, "/next", &next2), post(&net, "/next", &next1));
    assert_eq!(body(&r1), r#"{"step":2,"cell":"c1"}"#);
    assert_eq!(body(&r2), r#"{"step":2,"cell":"c2"}"#);
    assert!(c1.verdict().is_green() && c2.verdict().is_green());
    assert!(c1.is_attributable() && c2.is_attributable());
}

#[test]
fn an_open_without_a_bind_is_not_attributable() {
    let svc = ScriptedHttpService::new();
    let h = svc.add(opening(&[r#""a":1"#], "x"), HttpBindings::new()).unwrap();
    assert!(!h.is_attributable());
}

fn refused(script: HttpScript, bindings: HttpBindings) -> HttpScriptError {
    match ScriptedHttpService::new().add(script, bindings) {
        Ok(_) => panic!("add accepted a script it cannot serve"),
        Err(e) => e,
    }
}

fn step(reply: HttpReply) -> HttpReifiedStep {
    HttpReifiedStep::new(HttpRequestMatch::post("/x"), reply)
}

/// What a refusal must be.
type Expect = fn(&HttpScriptError) -> bool;

#[test]
fn add_refuses_scripts_that_cannot_be_served() {
    let open = || HttpRequestMatch::post("/x");
    let tail = || step(HttpReply::respond(200, ""));
    let cases: Vec<(HttpScript, Expect)> = vec![
        (HttpScript::reified(open(), vec![]), |e| matches!(e, HttpScriptError::NoStep)),
        (HttpScript::reified(open(), vec![step(HttpReply::Silence), tail()]), |e| {
            matches!(e, HttpScriptError::UnreachableStep { index: 1, previous: 0 })
        }),
        (HttpScript::reified(open(), vec![step(HttpReply::Reset.late(5)), tail()]), |e| {
            matches!(e, HttpScriptError::UnreachableStep { index: 1, previous: 0 })
        }),
        (HttpScript::reified(open(), vec![step(HttpReply::respond(200, "{}")), tail()]), |e| {
            matches!(e, HttpScriptError::UnreachableStep { index: 1, previous: 0 })
        }),
        (HttpScript::reified(open(), vec![step(HttpReply::respond(200, "${leg:a}"))]), |e| {
            matches!(e, HttpScriptError::UnknownPlaceholder { .. })
        }),
        (HttpScript::reified(open(), vec![step(HttpReply::respond(200, "${bind:egress"))]), |e| {
            matches!(e, HttpScriptError::UnknownPlaceholder { .. })
        }),
        (
            HttpScript::reified(open(), vec![step(HttpReply::respond(200, "${bind:nope}"))]),
            |e| matches!(e, HttpScriptError::UnknownBind { name, .. } if name == "nope"),
        ),
        (
            HttpScript::reified(open(), vec![step(HttpReply::respond(200, "${capture:id}"))]),
            |e| matches!(e, HttpScriptError::UnboundCapture { name, .. } if name == "id"),
        ),
        (HttpScript::reified(open().contains("${continuation}"), vec![tail()]), |e| {
            matches!(e, HttpScriptError::ContinuationInMatch { .. })
        }),
        (HttpScript::reified(open().contains("${capture:id}"), vec![tail()]), |e| {
            matches!(e, HttpScriptError::UnanchoredCapture { .. })
        }),
        (HttpScript::reified(HttpRequestMatch::post(""), vec![tail()]), |e| {
            matches!(e, HttpScriptError::EmptyTarget { .. })
        }),
    ];
    for (i, (script, expected)) in cases.into_iter().enumerate() {
        let err = refused(script, HttpBindings::new());
        assert!(expected(&err), "case {i}: unexpected refusal {err:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_reified_program_is_a_serde_document() {
    let document = r#"{
        "open": { "method": "POST", "path": "/start", "contains": ["\"cell\":\"${bind:cell}\""] },
        "step": { "reified": [
            { "expect": { "method": "POST", "path": "/start" },
              "reply": { "kind": "respond", "status": 200, "body": "{\"ctx\":\"${continuation}\"}" } },
            { "expect": { "method": "POST", "path": "/next" },
              "reply": { "kind": "late", "ms": 20, "then": { "kind": "respond", "status": 202 } } }
        ] }
    }"#;
    let script: HttpScript = serde_json::from_str(document).unwrap();
    let round: HttpScript = serde_json::from_str(&serde_json::to_string(&script).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&round).unwrap(),
        serde_json::to_value(&script).unwrap(),
        "a program survives its own serialisation"
    );

    let svc = ScriptedHttpService::new();
    let handle = svc.add(script, cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;
    let token = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    assert_eq!(post(&net, "/next", &format!(r#"{{"ctx":"{token}"}}"#)).await.status, 202);
    assert!(handle.verdict().is_green());

    let code = HttpScript::code(HttpRequestMatch::post("/x"), |_, _, _| {
        Ok((HttpReply::respond(200, ""), None))
    });
    assert!(serde_json::to_string(&code).is_err(), "a code step is not a document");
}

#[tokio::test(start_paused = true)]
async fn a_code_reply_that_does_not_render_is_unmatched() {
    let svc = ScriptedHttpService::new();
    let broken = HttpScript::code(HttpRequestMatch::post("/start"), |_, _, _| {
        Ok((HttpReply::respond(200, r#"{"id":"${capture:id}"}"#), None))
    });
    let script = svc.add(broken, HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    let resp = post(&net, "/start", "{}").await;
    assert_eq!(resp.status, 500);
    assert!(body(&resp).contains("does not render"), "{}", body(&resp));
    let verdict = script.verdict();
    assert_eq!(verdict.findings.len(), 1, "{verdict:?}");
    assert_eq!(verdict.findings[0].kind, HttpFindingKind::Unmatched);
}

#[tokio::test(start_paused = true)]
async fn the_same_token_echoed_twice_is_one_position() {
    let svc = ScriptedHttpService::new();
    let script = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let token = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    let resp =
        post(&net, "/next", &format!(r#"{{"cell":"c1","ctx":"{token}","copy":"{token}"}}"#)).await;
    assert_eq!(resp.status, 200, "{}", body(&resp));
    assert!(script.verdict().is_green());
}

// ── Review findings ─────────────────────────────────────────────────────────

/// A payload that is a well-formed token but for its checksum is the peer's
/// data: the request is an opening request, never a follow-up of instance 0.
#[tokio::test(start_paused = true)]
async fn a_token_with_a_wrong_checksum_is_user_data() {
    use base64::Engine;
    let svc = ScriptedHttpService::with_nonce(5);
    let first = svc
        .add(
            HttpScript::reified(
                HttpRequestMatch::post("/s"),
                vec![
                    HttpReifiedStep::new(
                        HttpRequestMatch::post("/s"),
                        HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
                    ),
                    HttpReifiedStep::new(
                        HttpRequestMatch::post("/s"),
                        HttpReply::respond(200, "second"),
                    ),
                ],
            ),
            HttpBindings::new(),
        )
        .unwrap();
    let second = svc.add(opening_at("/s", "opened-2"), HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;
    post(&net, "/s", "{}").await;

    // The wire form, written out: 8 checksum bytes (zeros, wrong) + a payload
    // naming this service's nonce, instance 0, position 1.
    let mut payload = vec![0u8; 8];
    payload.extend_from_slice(br#"{"n":5,"i":0,"a":{"k":"r","p":1}}"#);
    let forged =
        format!("~hc.{}.~", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload));
    let resp = post(&net, "/s", &format!(r#"{{"ctx":"{forged}"}}"#)).await;
    assert_eq!(body(&resp), "opened-2");
    assert!(second.verdict().opened);
    assert!(!first.verdict().complete, "the forged token did not advance instance 0");
}

fn opening_at(path: &str, reply: &str) -> HttpScript {
    HttpScript::reified(
        HttpRequestMatch::post(path),
        vec![HttpReifiedStep::new(HttpRequestMatch::post(path), HttpReply::respond(200, reply))],
    )
}

#[tokio::test(start_paused = true)]
async fn a_retransmit_after_completion_does_not_regress_the_instance() {
    let svc = ScriptedHttpService::new();
    let steps = vec![
        HttpReifiedStep::new(
            HttpRequestMatch::post("/s"),
            HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
        ),
        HttpReifiedStep::new(
            HttpRequestMatch::post("/n"),
            HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
        ),
        HttpReifiedStep::new(HttpRequestMatch::post("/l"), HttpReply::respond(204, "")),
    ];
    let script = svc
        .add(HttpScript::reified(HttpRequestMatch::post("/s"), steps), HttpBindings::new())
        .unwrap();
    let (net, _h) = serve(&svc).await;

    let t1 = ctx(&post(&net, "/s", "{}").await);
    let a = post(&net, "/n", &format!(r#"{{"ctx":"{t1}"}}"#)).await;
    post(&net, "/l", &format!(r#"{{"ctx":"{}"}}"#, ctx(&a))).await;
    let again = post(&net, "/n", &format!(r#"{{"ctx":"{t1}"}}"#)).await;
    assert_eq!(again, a, "the late retransmit is answered identically");
    assert!(script.verdict().complete, "an older token never regresses the instance");
}

/// Serve one request against a single-step script opening on `fragment`.
async fn one(fragment: &str, reply: &str, request: &str) -> HttpResponse {
    let svc = ScriptedHttpService::new();
    let script = HttpScript::reified(
        HttpRequestMatch::post("/s").contains(fragment),
        vec![HttpReifiedStep::new(HttpRequestMatch::post("/s"), HttpReply::respond(200, reply))],
    );
    svc.add(script, HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;
    post(&net, "/s", request).await
}

#[tokio::test(start_paused = true)]
async fn a_scalar_capture_takes_only_a_number_or_a_literal() {
    let fragment = r#""id":${capture:id}"#;
    let reply = r#"{"echo":${capture:id}}"#;
    for request in [r#"{"id":{"a":1},"z":2}"#, r#"{"id":[1,2],"z":2}"#, r#"{"id":"x,y","z":2}"#] {
        assert_eq!(one(fragment, reply, request).await.status, 500, "{request}");
    }
    for (request, echoed) in [
        (r#"{"id":-1.5e3,"z":2}"#, r#"{"echo":-1.5e3}"#),
        (r#"{"id":1.50}"#, r#"{"echo":1.50}"#),
        (r#"{"id":null}"#, r#"{"echo":null}"#),
        (r#"{"id":false}"#, r#"{"echo":false}"#),
    ] {
        let resp = one(fragment, reply, request).await;
        assert_eq!(body(&resp), echoed, "{request}");
    }
}

#[tokio::test(start_paused = true)]
async fn normalisation_drops_only_whitespace_outside_strings() {
    let e_acute = "caf\u{e9}";
    let matching = [
        (r#""x":1e3"#.to_string(), "{ \"x\" : 1e3 }".to_string()),
        (r#""x":1.50"#.to_string(), "{\n  \"x\": 1.50\n}".to_string()),
        (r#""u":"a\/b""#.to_string(), r#"{ "u": "a\/b" }"#.to_string()),
        (format!(r#""n":"{e_acute}""#), format!(r#"{{ "n": "{e_acute}" }}"#)),
        (r#""n":"café""#.to_string(), r#"{"n": "café"}"#.to_string()),
        (r#""r":{"b":1,"a":2}"#.to_string(), r#"{"r": {"b": 1, "a": 2}}"#.to_string()),
        (r#""id":"a""#.to_string(), r#"{"id":"a", "id":"b"}"#.to_string()),
        (r#""n":18446744073709551616"#.to_string(), r#"{"n": 18446744073709551616}"#.to_string()),
        (r#""s":"a b""#.to_string(), r#"{ "s": "a b" }"#.to_string()),
    ];
    for (fragment, request) in matching {
        let resp = one(&fragment, "ok", &request).await;
        assert_eq!(resp.status, 200, "{fragment} in {request}: {}", body(&resp));
    }
    let resp = one(r#""s":"ab""#, "ok", r#"{ "s": "a b" }"#).await;
    assert_eq!(resp.status, 500, "whitespace inside a string is data");
}

#[tokio::test(start_paused = true)]
async fn the_open_match_weighs_step_zero_expect_too() {
    let svc = ScriptedHttpService::new();
    let mk = |v: &str| {
        HttpScript::reified(
            HttpRequestMatch::post("/s"),
            vec![HttpReifiedStep::new(
                HttpRequestMatch::post("/s").contains(format!(r#""b":{v}"#)),
                HttpReply::respond(200, v),
            )],
        )
    };
    let one = svc.add(mk("1"), HttpBindings::new()).unwrap();
    let two = svc.add(mk("2"), HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    let resp = post(&net, "/s", r#"{"b":2}"#).await;
    assert_eq!(body(&resp), "2");
    assert!(two.verdict().is_green(), "{:?}", two.verdict());
    assert!(!one.verdict().opened, "the sibling step 0 refuses is not consumed");
    assert!(svc.findings().iter().all(|f| f.kind == HttpFindingKind::Unserved));
}

#[tokio::test(start_paused = true)]
async fn tokens_all_foreign_are_an_advisory_however_many() {
    let earlier = ScriptedHttpService::with_nonce(1);
    earlier.add(two_step(), cell("c1")).unwrap();
    earlier.add(two_step(), cell("c2")).unwrap();
    let (earlier_net, _e) = serve(&earlier).await;
    let s1 = ctx(&post(&earlier_net, "/start", r#"{"cell":"c1"}"#).await);
    let s2 = ctx(&post(&earlier_net, "/start", r#"{"cell":"c2"}"#).await);

    let svc = ScriptedHttpService::with_nonce(2);
    let (net, _h) = serve(&svc).await;
    let resp = post(&net, "/next", &format!(r#"{{"ctx":"{s1}","history":["{s2}"]}}"#)).await;
    assert_eq!(resp.status, 500);
    let findings = svc.findings();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].kind, HttpFindingKind::ForeignToken);
    assert!(findings[0].is_advisory());
}

#[tokio::test(start_paused = true)]
async fn opens_differing_only_in_capture_names_are_one_set() {
    let svc = ScriptedHttpService::new();
    let x = svc
        .add(
            HttpScript::reified(
                HttpRequestMatch::post("/s").contains(r#""id":"${capture:x}""#),
                vec![HttpReifiedStep::new(
                    HttpRequestMatch::post("/s"),
                    HttpReply::respond(200, "x:${capture:x}"),
                )],
            ),
            HttpBindings::new(),
        )
        .unwrap();
    let y = svc
        .add(
            HttpScript::reified(
                HttpRequestMatch::post("/s").contains(r#""id":"${capture:y}""#),
                vec![HttpReifiedStep::new(
                    HttpRequestMatch::post("/s"),
                    HttpReply::respond(200, "y:${capture:y}"),
                )],
            ),
            HttpBindings::new(),
        )
        .unwrap();
    let (net, _h) = serve(&svc).await;

    assert_eq!(body(&post(&net, "/s", r#"{"id":"abc"}"#).await), "x:abc", "FIFO");
    assert_eq!(body(&post(&net, "/s", r#"{"id":"def"}"#).await), "y:def");
    assert!(x.verdict().is_green() && y.verdict().is_green());
}

#[tokio::test(start_paused = true)]
async fn a_literal_fragment_is_more_specific_than_a_capture_on_the_same_key() {
    let svc = ScriptedHttpService::new();
    let any = svc
        .add(
            HttpScript::reified(
                HttpRequestMatch::post("/s").contains(r#""id":"${capture:x}""#),
                vec![HttpReifiedStep::new(
                    HttpRequestMatch::post("/s"),
                    HttpReply::respond(200, "any"),
                )],
            ),
            HttpBindings::new(),
        )
        .unwrap();
    let abc = svc.add(opening(&[r#""id":"abc""#], "abc"), HttpBindings::new()).unwrap();
    let (net, _h) = serve(&svc).await;

    assert_eq!(body(&post(&net, "/s", r#"{"id":"abc"}"#).await), "abc", "the literal wins");
    assert_eq!(body(&post(&net, "/s", r#"{"id":"zzz"}"#).await), "any");
    assert!(any.verdict().is_green() && abc.verdict().is_green());
}

#[test]
fn add_refuses_what_a_server_cannot_send() {
    let bad =
        |reply: HttpReply| HttpScript::reified(HttpRequestMatch::post("/x"), vec![step(reply)]);
    for status in [42, 99, 600, 1000] {
        let err = refused(bad(HttpReply::respond(status, "")), HttpBindings::new());
        assert!(matches!(err, HttpScriptError::InvalidStatus { .. }), "{status}: {err:?}");
    }
    for (name, value) in [("bad name", "v"), ("", "v"), ("x-ok", "a\r\nb"), ("x-ok", "caf\u{e9}")] {
        let err =
            refused(bad(HttpReply::respond(200, "").header(name, value)), HttpBindings::new());
        assert!(matches!(err, HttpScriptError::InvalidHeader { .. }), "{name:?}: {err:?}");
    }
    let err = refused(
        bad(HttpReply::respond(200, "").header("x-ok", "${bind:v}")),
        HttpBindings::new().bind("v", "a\nb"),
    );
    assert!(matches!(err, HttpScriptError::InvalidHeader { .. }), "a bound value: {err:?}");
    let err = refused(
        HttpScript::reified(
            HttpRequestMatch::post("/start"),
            vec![HttpReifiedStep::new(
                HttpRequestMatch::post("/other"),
                HttpReply::respond(200, ""),
            )],
        ),
        HttpBindings::new(),
    );
    assert!(matches!(err, HttpScriptError::StepZeroTarget { .. }), "{err:?}");
}

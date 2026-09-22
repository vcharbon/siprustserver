//! The continuation codec: the peer may return the opaque value under a
//! peer-specific wrapping. The codec wraps the token on emission and unwraps
//! candidates from a request body on the scan; the raw scan still runs.
//!
//! The test codec wraps a token as `hex:` + the hex of its bytes, a form in
//! which the token's own delimiters never appear on the wire.

use std::sync::{Arc, Mutex};

use http_net::scripted::{
    HttpBindings, HttpContinuationCodec, HttpFindingKind, HttpReifiedStep, HttpReply,
    HttpRequestMatch, HttpScript, IdentityCodec, ScriptedHttpService,
};
use http_net::{HttpRequest, HttpResponse, HttpServerHandle, HttpTransport, SimulatedHttpNetwork};

const PREFIX: &str = "hex:";

struct HexCodec;

impl HttpContinuationCodec for HexCodec {
    fn wrap(&self, token: &str, _reply: &str) -> String {
        let hex: String = token.bytes().map(|b| format!("{b:02x}")).collect();
        format!("{PREFIX}{hex}")
    }

    fn unwrap(&self, body: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(body);
        text.match_indices(PREFIX)
            .filter_map(|(at, _)| {
                let run: String =
                    text[at + PREFIX.len()..].chars().take_while(char::is_ascii_hexdigit).collect();
                let bytes: Option<Vec<u8>> = (0..run.len() / 2)
                    .map(|i| u8::from_str_radix(&run[2 * i..2 * i + 2], 16).ok())
                    .collect();
                String::from_utf8(bytes?).ok()
            })
            .collect()
    }
}

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

fn ctx(resp: &HttpResponse) -> String {
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    v["ctx"].as_str().unwrap().to_string()
}

fn text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn two_step() -> HttpScript {
    HttpScript::reified(
        HttpRequestMatch::post("/start").contains(r#""cell":"${bind:cell}""#),
        vec![
            HttpReifiedStep::new(
                HttpRequestMatch::post("/start"),
                HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
            ),
            HttpReifiedStep::new(
                HttpRequestMatch::post("/next"),
                HttpReply::respond(200, r#"{"done":true}"#),
            ),
        ],
    )
}

fn cell(name: &str) -> HttpBindings {
    HttpBindings::new().bind("cell", name)
}

#[tokio::test(start_paused = true)]
async fn the_reply_carries_the_wrapped_token_and_the_wrapped_echo_is_followed() {
    let svc = ScriptedHttpService::with_codec(Arc::new(HexCodec));
    let handle = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let first = post(&net, "/start", r#"{"cell":"c1"}"#).await;
    let wrapped = ctx(&first);
    assert!(wrapped.starts_with(PREFIX), "the token travels wrapped: {wrapped}");
    assert!(!text(&first).contains("~hc."), "no raw token on the wire: {}", text(&first));

    let second = post(&net, "/next", &format!(r#"{{"ctx":"{wrapped}"}}"#)).await;
    assert_eq!(second.status, 200, "{}", text(&second));
    assert!(handle.verdict().is_green(), "{:?}", handle.verdict());
}

#[tokio::test(start_paused = true)]
async fn a_retransmitted_wrapped_request_is_answered_identically() {
    let svc = ScriptedHttpService::with_nonce_and_codec(11, Arc::new(HexCodec));
    let handle = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let wrapped = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    let body = format!(r#"{{"ctx":"{wrapped}"}}"#);
    let once = post(&net, "/next", &body).await;
    let again = post(&net, "/next", &body).await;
    assert_eq!(once, again);
    assert!(handle.verdict().is_green(), "{:?}", handle.verdict());
}

#[tokio::test(start_paused = true)]
async fn the_raw_scan_stays_the_fallback_under_a_codec() {
    let svc = ScriptedHttpService::with_codec(Arc::new(HexCodec));
    let handle = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let wrapped = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    let raw = HexCodec.unwrap(format!(r#""{wrapped}""#).as_bytes()).remove(0);
    assert!(raw.starts_with("~hc."), "the unwrapped form is the raw token: {raw}");

    let second = post(&net, "/next", &format!(r#"{{"ctx":"{raw}"}}"#)).await;
    assert_eq!(second.status, 200, "{}", text(&second));
    assert!(handle.verdict().is_green(), "{:?}", handle.verdict());
}

#[tokio::test(start_paused = true)]
async fn the_same_token_raw_and_wrapped_is_one_position() {
    let svc = ScriptedHttpService::with_codec(Arc::new(HexCodec));
    let handle = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let wrapped = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    let raw = HexCodec.unwrap(wrapped.as_bytes()).remove(0);
    let second = post(&net, "/next", &format!(r#"{{"a":"{wrapped}","b":"{raw}"}}"#)).await;
    assert_eq!(second.status, 200, "{}", text(&second));
    assert!(handle.verdict().is_green(), "{:?}", handle.verdict());
}

/// A codec that remembers what it unwrapped, so a test sees the scan read it.
struct Watched {
    seen: Arc<Mutex<Vec<String>>>,
}

impl HttpContinuationCodec for Watched {
    fn wrap(&self, token: &str, reply: &str) -> String {
        HexCodec.wrap(token, reply)
    }

    fn unwrap(&self, body: &[u8]) -> Vec<String> {
        let found = HexCodec.unwrap(body);
        self.seen.lock().unwrap().extend(found.iter().cloned());
        found
    }
}

#[tokio::test(start_paused = true)]
async fn a_wrapped_look_alike_that_unwraps_to_no_token_is_user_data() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let svc = ScriptedHttpService::with_codec(Arc::new(Watched { seen: seen.clone() }));
    let handle = svc.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    // `hex:` + the hex of text shaped like a token, whose checksum fails.
    let decoy = HexCodec.wrap("~hc.AAAAAAAAAAAAAAAAAAAAAA.~", "");
    let resp = post(&net, "/start", &format!(r#"{{"cell":"c1","note":"{decoy}"}}"#)).await;
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        ["~hc.AAAAAAAAAAAAAAAAAAAAAA.~"],
        "the scan unwrapped the decoy"
    );
    assert_eq!(resp.status, 200, "and took it for data: the request opens: {}", text(&resp));
    assert!(handle.verdict().opened);
}

#[tokio::test(start_paused = true)]
async fn a_wrapped_token_of_another_service_is_a_foreign_advisory() {
    let theirs = ScriptedHttpService::with_nonce_and_codec(1, Arc::new(HexCodec));
    theirs.add(two_step(), cell("c1")).unwrap();
    let (their_net, _t) = serve(&theirs).await;
    let wrapped = ctx(&post(&their_net, "/start", r#"{"cell":"c1"}"#).await);

    let ours = ScriptedHttpService::with_nonce_and_codec(2, Arc::new(HexCodec));
    ours.add(two_step(), cell("c1")).unwrap();
    let (net, _h) = serve(&ours).await;
    let resp = post(&net, "/next", &format!(r#"{{"ctx":"{wrapped}"}}"#)).await;
    assert_eq!(resp.status, 500, "{}", text(&resp));
    let findings = ours.findings();
    let foreign: Vec<_> =
        findings.iter().filter(|f| f.kind == HttpFindingKind::ForeignToken).collect();
    assert_eq!(foreign.len(), 1, "{findings:?}");
    assert!(foreign[0].is_advisory() && foreign[0].instances.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_wrapped_own_token_beside_a_different_raw_own_token_is_ambiguous() {
    let svc = ScriptedHttpService::with_codec(Arc::new(HexCodec));
    let first = svc.add(two_step(), cell("c1")).unwrap();
    let second = svc.add(two_step(), cell("c2")).unwrap();
    let (net, _h) = serve(&svc).await;

    let wrapped = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    let other = ctx(&post(&net, "/start", r#"{"cell":"c2"}"#).await);
    let raw = HexCodec.unwrap(other.as_bytes()).remove(0);
    let resp = post(&net, "/next", &format!(r#"{{"a":"{wrapped}","b":"{raw}"}}"#)).await;
    assert_eq!(resp.status, 500, "{}", text(&resp));
    for handle in [&first, &second] {
        let findings = handle.verdict().findings;
        assert!(findings.iter().any(|f| f.kind == HttpFindingKind::Ambiguous), "{findings:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn fragments_match_the_body_as_sent_never_the_unwrapped_token() {
    let svc = ScriptedHttpService::with_codec(Arc::new(HexCodec));
    let script = HttpScript::reified(
        HttpRequestMatch::post("/start").contains(r#""cell":"${bind:cell}""#),
        vec![
            HttpReifiedStep::new(
                HttpRequestMatch::post("/start"),
                HttpReply::respond(200, r#"{"ctx":"${continuation}"}"#),
            ),
            // A fragment on the raw token's opening delimiter: never in the
            // body as sent, which carries the wrapped form only.
            HttpReifiedStep::new(
                HttpRequestMatch::post("/next").contains(r#""ctx":"~hc."#),
                HttpReply::respond(200, r#"{"done":true}"#),
            ),
        ],
    );
    let handle = svc.add(script, cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let wrapped = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    let second = post(&net, "/next", &format!(r#"{{"ctx":"{wrapped}"}}"#)).await;
    assert_eq!(second.status, 500, "the fragment reads the body as sent: {}", text(&second));
    let findings = handle.verdict().findings;
    assert!(findings.iter().any(|f| f.kind == HttpFindingKind::Unmatched), "{findings:?}");
}

#[test]
fn the_identity_codec_wraps_nothing_and_unwraps_nothing() {
    assert_eq!(IdentityCodec.wrap("~hc.abc.~", "{}"), "~hc.abc.~");
    assert!(IdentityCodec.unwrap(b"{\"ctx\":\"~hc.abc.~\"}").is_empty());
}

/// A codec whose wrapping echoes a field of the reply it stands in, as a peer
/// whose context mirrors its own answer does; it records the replies it saw.
struct Echoing {
    replies: Arc<Mutex<Vec<String>>>,
}

impl HttpContinuationCodec for Echoing {
    fn wrap(&self, token: &str, reply: &str) -> String {
        self.replies.lock().unwrap().push(reply.to_string());
        let reply: serde_json::Value = serde_json::from_str(reply).unwrap();
        HexCodec.wrap(&format!("{}|{token}", reply["cell"].as_str().unwrap_or("-")), "")
    }

    fn unwrap(&self, body: &[u8]) -> Vec<String> {
        HexCodec
            .unwrap(body)
            .into_iter()
            .map(|t| t.split_once('|').map_or(t.clone(), |(_, token)| token.to_string()))
            .collect()
    }
}

#[tokio::test(start_paused = true)]
async fn the_wrapping_reads_the_reply_it_stands_in_rendered_without_the_continuation() {
    let replies = Arc::new(Mutex::new(Vec::new()));
    let svc = ScriptedHttpService::with_codec(Arc::new(Echoing { replies: replies.clone() }));
    let script = HttpScript::reified(
        HttpRequestMatch::post("/start").contains(r#""cell":"${bind:cell}""#),
        vec![
            HttpReifiedStep::new(
                HttpRequestMatch::post("/start"),
                HttpReply::respond(200, r#"{"cell":"${bind:cell}","ctx":"${continuation}"}"#),
            ),
            HttpReifiedStep::new(
                HttpRequestMatch::post("/next"),
                HttpReply::respond(200, r#"{"done":true}"#),
            ),
        ],
    );
    let handle = svc.add(script, cell("c1")).unwrap();
    let (net, _h) = serve(&svc).await;

    let wrapped = ctx(&post(&net, "/start", r#"{"cell":"c1"}"#).await);
    assert_eq!(
        replies.lock().unwrap().first().map(String::as_str),
        Some(r#"{"cell":"c1","ctx":""}"#),
        "the codec saw the reply, bindings resolved and the continuation empty"
    );
    let echoed = HexCodec.unwrap(format!(r#""{wrapped}""#).as_bytes()).remove(0);
    assert!(echoed.starts_with("c1|~hc."), "the wrapping echoes the reply's field: {echoed}");

    let second = post(&net, "/next", &format!(r#"{{"ctx":"{wrapped}"}}"#)).await;
    assert_eq!(second.status, 200, "{}", text(&second));
    assert!(handle.verdict().is_green(), "{:?}", handle.verdict());
}

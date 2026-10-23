//! A failover consult's reject is held to the initial consult's guard: a code
//! that is no non-2xx final (RFC 3261 §21.3–§21.6) rejects nothing, and the
//! caller is answered the plain server error instead. A refused reject or
//! redirect keeps the decision's headers on that 500.

use std::sync::Arc;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{B2buaScene, B2buaSut, BOB_PORT};
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// One route to bob, stating sets of its own; on its failure the plan
/// answers `on_exhausted`.
fn plan(on_exhausted: serde_json::Value) -> String {
    serde_json::json!({
        "action": "route",
        "routes": [{"destination": {"host": "127.0.0.1", "port": BOB_PORT},
                    "stated_headers": {
                        "originator_finals": {"X-Final": ["route"]},
                        "every_message": {"X-Every": ["route"]}}}],
        "on_exhausted": on_exhausted,
    })
    .to_string()
}

/// The final alice gets once bob refuses the call `plan` routes to him.
async fn refused_final(name: &str, plan: &str, status: u16) -> sip_message::SipResponse {
    let s = B2buaScene::with_b2bua(name, |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
    })
    .await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", plan)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.bob.receive("INVITE").await.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    let resp = call.expect(status).await;
    let _report = s.finish().await;
    resp
}

#[tokio::test(start_paused = true)]
async fn a_failover_reject_that_is_no_final_draws_the_plain_server_error() {
    for code in [180, 200] {
        let reject = serde_json::json!({"action": "reject", "code": code, "reason": "Not A Final"});
        let resp = refused_final(&format!("failover-reject-{code}"), &plan(reject), 500).await;
        assert_eq!(resp.reason(), "Server Internal Error", "a reject stating {code}");
    }
}

/// A failover reject or redirect the stack refuses — a reject that is no
/// non-2xx final, a redirect that is not a 3xx or whose target does not read
/// — draws the plain server error, which keeps the decision's headers as its
/// decided final would: its updates and its adds ride the 500, and the
/// route's set ends.
#[tokio::test(start_paused = true)]
async fn a_refused_failover_decision_keeps_its_headers_on_the_plain_server_error() {
    let headers = serde_json::json!({"X-Set": "v", "X-Decision": {"add": ["r"]}});
    for (name, decision) in [
        ("reject-not-a-final", serde_json::json!({"action": "reject", "code": 200})),
        (
            "redirect-not-a-3xx",
            serde_json::json!({"action": "redirect", "code": 404,
                "contacts": [{"uri": "sip:target@192.0.2.9"}]}),
        ),
        (
            "redirect-unreadable-target",
            serde_json::json!({"action": "redirect", "code": 302,
                "contacts": [{"uri": "not a uri at all"}]}),
        ),
    ] {
        let mut decision = decision;
        decision["update_headers"] = headers.clone();
        let refused = refused_final(&format!("refused-{name}"), &plan(decision), 500).await;
        let lines = |n: &str| -> Vec<String> {
            refused.raw(HeaderName::from(n)).map(str::to_string).collect()
        };
        assert_eq!(lines("X-Set"), ["v"], "{name}: the decision's update rides the 500");
        assert_eq!(lines("X-Decision"), ["r"], "{name}: the decision's add rides the 500");
        assert!(lines("X-Every").is_empty(), "{name}: the route's set ended");
        assert!(lines("X-Final").is_empty(), "{name}: the route's finals set ended");
    }
}

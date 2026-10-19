//! A failover consult's reject is held to the initial consult's guard: a code
//! that is no non-2xx final (RFC 3261 §21.3–§21.6) rejects nothing, and the
//! caller is answered the plain server error instead.

use std::sync::Arc;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{B2buaScene, B2buaSut, BOB_PORT};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// One route to bob; on its failure the plan rejects with `code`.
fn plan(code: u16) -> String {
    serde_json::json!({
        "action": "route",
        "routes": [{"destination": {"host": "127.0.0.1", "port": BOB_PORT}}],
        "on_exhausted": {"action": "reject", "code": code, "reason": "Not A Final"},
    })
    .to_string()
}

#[tokio::test(start_paused = true)]
async fn a_failover_reject_that_is_no_final_draws_the_plain_server_error() {
    for code in [180, 200] {
        let s = B2buaScene::with_b2bua(&format!("failover-reject-{code}"), |_bob_port| {
            B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
        })
        .await;
        let mut call = s
            .alice
            .invite(&s.bob)
            .with_sdp(OFFER)
            .with_header("X-Api-Call", &plan(code))
            .through(s.b2bua.addr)
            .send()
            .await;
        s.bob.receive("INVITE").await.respond(486, "Busy Here").await;
        s.bob.receive("ACK").await;

        let resp = call.expect(500).await;
        assert_eq!(resp.reason(), "Server Internal Error", "a reject stating {code}");
        let _report = s.finish().await;
    }
}

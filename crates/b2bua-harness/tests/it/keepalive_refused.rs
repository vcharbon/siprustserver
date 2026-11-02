//! Keepalive where one peer refuses the probe with a failure other than 481.
//!
//! Long call; the keepalive fires and the B2BUA sends an in-dialog OPTIONS to
//! both legs. Alice answers 200; bob answers 503. A refused probe is a failed
//! probe (RFC 3261 §11: OPTIONS is answered as an INVITE would be), so the call
//! ends at once under the keepalive deadline, by bob's leg, and both legs are
//! BYEd (§12.2.1.2) — the probe's Timer F is not waited for.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene};
use call::CdrEventType;

/// The harness keepalive interval.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn a_probe_refused_with_503_ends_the_call_at_once() {
    let s = B2buaScene::new("b2bua-keepalive-refused").await;
    let _dialog = s.establish().await;

    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(503, "Service Unavailable").await;

    // Both BYEs leave on the refusal, well inside the probe's Timer F.
    s.h.advance(Duration::from_secs(1)).await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    s.bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the call");
    let bye = cdrs[0].events.iter().find(|e| e.event_type == CdrEventType::Bye);
    assert_eq!(bye.and_then(|e| e.status_code), Some(503), "the refusal is recorded: {cdrs:?}");
    let json = serde_json::to_value(&cdrs[0]).expect("the CDR serialises");
    assert_eq!(json["termination"]["cause"], serde_json::json!({"timeout": "keepalive"}));
    assert_eq!(json["termination"]["by_leg"], serde_json::json!("b-1"));
    let _report = s.finish().await;
}

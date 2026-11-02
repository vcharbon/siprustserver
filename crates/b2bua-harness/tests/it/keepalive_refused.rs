//! Keepalive where one peer refuses the probe.
//!
//! The probe belongs to no usage of the dialog (RFC 5057 §5.3). A refusal that
//! keeps the dialog (503) fails the probe alone: the call stays up and the
//! next interval probes again. A refusal that destroys the dialog (404, RFC
//! 5057 §5.1 Table 1) ends the call at once under the keepalive deadline, by
//! the refusing leg, both legs BYEd (RFC 3261 §12.2.1.2).

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene};
use call::CdrEventType;

/// The harness keepalive interval.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn a_probe_refused_with_503_leaves_the_call_up() {
    let s = B2buaScene::new("b2bua-keepalive-refused-503").await;
    let mut dialog = s.establish().await;

    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(503, "Service Unavailable").await;

    // The next interval probes again; both answer.
    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(200, "OK").await;
    assert_eq!(s.b2bua.active_calls(), 1, "the refusal failed the probe alone");

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    let _report = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_probe_refused_with_404_ends_the_call_at_once() {
    let s = B2buaScene::new("b2bua-keepalive-refused-404").await;
    let _dialog = s.establish().await;

    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(404, "Not Found").await;

    // Both BYEs leave on the refusal, well inside the probe's Timer F.
    s.h.advance(Duration::from_secs(1)).await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    s.bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the call");
    let bye = cdrs[0].events.iter().find(|e| e.event_type == CdrEventType::Bye);
    assert_eq!(bye.and_then(|e| e.status_code), Some(404), "the refusal is recorded: {cdrs:?}");
    let json = serde_json::to_value(&cdrs[0]).expect("the CDR serialises");
    assert_eq!(json["termination"]["cause"], serde_json::json!({"timeout": "keepalive"}));
    assert_eq!(json["termination"]["by_leg"], serde_json::json!("b-1"));
    let _report = s.finish().await;
}

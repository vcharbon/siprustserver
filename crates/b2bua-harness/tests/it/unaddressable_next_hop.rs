//! A b-leg INVITE whose next hop is not an IP literal fails at once: the
//! worker resolves no name, so with no outbound proxy configured an
//! allow-listed name destination cannot be put on the wire, and the call takes
//! its dead-hop response-timeout path at once instead of waiting on a B2BUA
//! timer.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;

/// Admitted by the default allow-list (`.svc.cluster.local`), never resolved.
const NAMED_CALLEE: &str = "callee.ns.svc.cluster.local";

/// alice is answered 408 in the turn right after the INVITE's; nothing
/// reaches bob. The
/// CANCELed Trying b-leg stays unresolved, as on a Timer B give-up, so the
/// call is reaped at its terminating safety timeout (32 s).
#[tokio::test(start_paused = true)]
async fn a_b_leg_invite_to_a_name_fails_the_attempt_at_once() {
    let s = B2buaScene::with_b2bua("unaddressable-next-hop", |bob_port| {
        B2buaSut::route_all_to(NAMED_CALLEE, bob_port)
    })
    .await;
    let started = tokio::time::Instant::now();

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    call.expect(408).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "an unaddressable INVITE fails at once, not on a timer: {elapsed:?}"
    );
    assert_eq!(
        s.b2bua.metrics().outbound_unaddressable_of("request"),
        2,
        "the INVITE and the CANCEL that abandons its leg are counted unsent"
    );

    s.h.advance(Duration::from_secs(33)).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call's CDR is written");
    s.finish().await;
}

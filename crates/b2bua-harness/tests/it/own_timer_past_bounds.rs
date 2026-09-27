//! A call's own timer fires once: nothing sends it again, and the call arms
//! each timer once until the fire runs. So the per-call dispatcher never
//! drops one for want of room: it waits past a full queue, and the call acts
//! on it once its worker frees.

use std::time::Duration;

use b2bua_harness::settle_until;
use scenario_harness::callflow::OFFER_SDP;
use sip_message::generators::InDialogMethod;

use crate::common::unrun::one_permit_one_deep_with;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// While a parked call holds the only handler permit, two INFOs fill alice's
/// call's worker and queue, and the call's keepalive comes due there. It
/// waits past the full queue: once the permit frees, both legs are probed,
/// and the keepalive re-arms for the next interval.
#[tokio::test(start_paused = true)]
async fn a_keepalive_due_on_a_full_per_call_queue_still_probes_both_legs() {
    let s = one_permit_one_deep_with("b2bua-keepalive-queue-full", |c| {
        c.call_control_timeout_ms = 40_000;
        c.keepalive_interval_sec = KEEPALIVE_INTERVAL.as_secs() as i64;
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut dialog = s.establish().await;

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info1 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    let mut info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(KEEPALIVE_INTERVAL).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 0, "the keepalive fire is not dropped");
    assert_eq!(s.b2bua.metrics().past_bound_total(), 1, "it waits past the full queue");
    s.h.advance(Duration::from_secs(9)).await;

    // The permit frees: the INFOs are relayed, then the keepalive probes both
    // legs.
    parked.expect(503).await;
    for _ in 0..2 {
        s.bob.receive("INFO").await.respond(200, "OK").await;
    }
    s.alice
        .try_receive("OPTIONS")
        .await
        .expect("the keepalive due on the full queue probes alice")
        .respond(200, "OK")
        .await;
    s.bob
        .try_receive("OPTIONS")
        .await
        .expect("the keepalive due on the full queue probes bob")
        .respond(200, "OK")
        .await;
    info1.expect(200).await;
    info2.expect(200).await;

    // It re-armed: the next interval probes both legs again.
    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(200, "OK").await;

    // The queue is one deep: the 200s run before the BYE is offered.
    s.h.advance(Duration::from_millis(300)).await;
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

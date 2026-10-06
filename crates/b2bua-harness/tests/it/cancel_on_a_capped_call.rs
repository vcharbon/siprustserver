//! A caller CANCELs a ringing call that has already crossed its lifetime
//! message cap. The transaction layer answers the CANCEL 200 and the INVITE
//! 487 (RFC 3261 §9.2); the call's end is then the cap's teardown. On the
//! wire the call still ends as a cancelled setup: the callee's INVITE is
//! CANCELed and its 487 ACKed, the caller hears no other final after its 487
//! and no BYE, the call is reaped and writes one CDR, which records no answer
//! and no final to the caller other than the one the caller received.

use b2bua_harness::{invite_final_statuses, settle_until};
use call::CdrEventType;
use scenario_harness::callflow::OFFER_SDP;
use std::time::Duration;

use crate::common::unrun::one_permit_one_deep_with;

const LIFETIME_CAP: u64 = 10;

/// bob rings; carol's call parks on its decision and holds the only handler
/// permit; bob's provisionals carry alice's call past its lifetime cap while
/// its worker waits. alice then CANCELs. Once carol's decision deadline frees
/// the permit, the call ends: bob is CANCELed, alice sees nothing more.
#[tokio::test(start_paused = true)]
async fn a_caller_cancel_on_a_capped_ringing_call_ends_it_as_a_cancelled_setup() {
    let s = one_permit_one_deep_with("b2bua-cancel-on-a-capped-call", |c| {
        c.max_messages_per_call_lifetime = LIFETIME_CAP;
        c.reaper_sweep_interval_sec = 3600;
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let metrics = s.b2bua.metrics();

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut b_invite = s.bob.receive("INVITE").await;
    b_invite.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    for _ in 0..LIFETIME_CAP {
        b_invite.respond(180, "Ringing").await;
    }
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(metrics.message_cap_lifetime_crossed_total(), 1, "the cap is crossed");
    assert_eq!(s.b2bua.active_calls(), 2, "the capped call has not ended yet");

    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    let terminated = call.expect(487).await;
    let call_id = terminated.call_id().as_str().to_string();
    s.h.advance(Duration::from_millis(300)).await;

    // carol's decision deadline frees the permit; the capped call's teardown
    // runs and CANCELs bob.
    parked.expect(503).await;
    let mut b_cancel =
        s.bob.try_receive("CANCEL").await.expect("the callee is CANCELed, not left ringing");
    b_cancel.respond(200, "OK").await;
    b_invite.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    settle_until(|| s.b2bua.cdr_records().len() == 2).await;
    let cdrs = s.b2bua.cdr_records();
    let mine: Vec<_> = cdrs.iter().filter(|c| c.a_leg.call_id == call_id).collect();
    assert_eq!(mine.len(), 1, "one CDR for the cancelled call: {cdrs:#?}");
    let cdr = mine[0];
    assert!(
        cdr.events.iter().all(|e| e.event_type != CdrEventType::Answer),
        "the call was never answered: {:#?}",
        cdr.events
    );
    assert!(cdr.termination.is_some(), "the CDR states how the call ended: {cdr:#?}");
    let a_leg_finals: Vec<i64> = cdr
        .events
        .iter()
        .filter(|e| e.leg_id == "a")
        .filter_map(|e| e.status_code.filter(|&code| code >= 200))
        .filter(|&code| code != 487)
        .collect();
    assert!(
        a_leg_finals.is_empty(),
        "the CDR records no final to the caller but the 487 it heard: {:#?}",
        cdr.events
    );

    let (b2bua_addr, alice_addr) = (s.b2bua.addr, s.alice.addr());
    let report = s.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice_addr),
        vec![487],
        "the caller hears its 487 and no other final"
    );
    let byes_to_alice = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua_addr && e.to == alice_addr && e.raw.starts_with(b"BYE "))
        .count();
    assert_eq!(byes_to_alice, 0, "no BYE on the cancelled caller's leg");
}

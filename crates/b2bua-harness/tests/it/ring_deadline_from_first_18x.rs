//! A route's ring deadline (`no_answer_ms`) runs on each dialled leg from the
//! first 180–189 provisional that leg's INVITE draws, to the millisecond: the
//! callee is CANCELled (RFC 3261 §9.1) exactly `no_answer_ms` after the stack
//! received that provisional.
//!
//! - A 183 is a first 18x like a 180; a later 18x of the leg, on its own early
//!   dialog or a sibling fork's (§12.1), does not move the deadline.
//! - A leg that answers, or fails, before any 18x never arms it.
//! - A failover leg and a transfer target each run their own from their own
//!   first 18x.
//!
//! Every hop takes [`TRANSIT_MS`], so a callee that sends its first 18x at `t`
//! receives the CANCEL at `t + TRANSIT_MS + no_answer_ms + TRANSIT_MS`.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallTreatment, NewCallResponse, RejectDecision, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, B2buaSut};
use call::TimerType;
use scenario_harness::{Agent, ClientInvite, Harness};
use sip_message::generators::InDialogMethod;
use tokio::time::Instant;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const FROM_TAG: &str = "alice-tag";

/// One hop between any two parties.
const TRANSIT_MS: u64 = 10;

/// The ring deadline every route here states: not a whole second.
const RING_MS: u64 = 2_345;

/// How long a callee waits after the INVITE before its first 18x: what the
/// deadline must not count.
const SILENCE: Duration = Duration::from_millis(700);

/// From a callee's first 18x leaving to the CANCEL reaching it.
const FIRST_18X_TO_CANCEL: Duration = Duration::from_millis(TRANSIT_MS + RING_MS + TRANSIT_MS);

/// Every call routes to `port` under the [`RING_MS`] ring deadline. With
/// `failover_to`, the first failure consults the backend, which routes to that
/// port under the same deadline; a later one is refused 480.
fn ringing_route(port: u16, failover_to: Option<u16>) -> Arc<dyn CallDecisionEngine> {
    let mut builder = ScriptedDecisionEngine::builder().fallback(move |_| {
        let mut r = route_to("127.0.0.1", port);
        r.no_answer_ms = Some(RING_MS as i64);
        if failover_to.is_some() {
            r.callback_context = Some("ring-deadline".into());
        }
        NewCallResponse::Route(r)
    });
    if let Some(next) = failover_to {
        builder = builder.on_failure(move |req| {
            if req.failure.failed_leg_id.as_deref() == Some("b-1") {
                let mut r = route_to("127.0.0.1", next);
                r.no_answer_ms = Some(RING_MS as i64);
                return CallTreatment::Route(r);
            }
            CallTreatment::Reject(RejectDecision {
                reject_code: 480,
                reject_reason: Some("Temporarily Unavailable".into()),
                update_headers: None,
                service_ext: Default::default(),
                label: None,
            })
        });
    }
    Arc::new(builder.build())
}

struct Scene {
    h: Harness,
    alice: Agent,
    bob: Agent,
    carol: Agent,
    b2bua: B2buaSut,
    call_ref: String,
}

/// alice, bob and carol around the stack under [`ringing_route`]; carol is
/// the failover callee when `failover` is set.
async fn scene(name: &str, failover: bool) -> Scene {
    let h = Harness::with_transit_delay(name, TRANSIT_MS);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let carol = h.agent_auto("carol").await;
    let decision = ringing_route(bob.addr().port(), failover.then(|| carol.addr().port()));
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let call_ref = call::derive_call_ref("w0", &call_id(name), FROM_TAG);
    Scene { h, alice, bob, carol, b2bua, call_ref }
}

fn call_id(name: &str) -> String {
    format!("{name}@127.0.0.1")
}

/// alice's INVITE toward bob, under the scene's call identity.
async fn dial(s: &Scene, name: &str) -> ClientInvite {
    s.alice
        .invite(&s.bob)
        .identity(call_id(name), FROM_TAG)
        .with_sdp(OFFER)
        .through(s.b2bua.addr)
        .send()
        .await
}

/// The ring deadlines the live call holds.
fn ring_deadlines(s: &Scene) -> usize {
    s.b2bua
        .live_call(&s.call_ref)
        .map_or(0, |c| c.timers.iter().filter(|t| *t.timer_type() == TimerType::NoAnswer).count())
}

/// The callee rings 700 ms after the INVITE: the CANCEL reaches it exactly
/// `no_answer_ms` after the stack took its 180, not after the INVITE.
#[tokio::test(start_paused = true)]
async fn the_ring_deadline_runs_from_the_first_18x_to_the_millisecond() {
    const NAME: &str = "ring-from-first-18x";
    let s = scene(NAME, false).await;
    let mut call = dial(&s, NAME).await;
    let mut uas = s.bob.receive("INVITE").await;
    s.h.advance(SILENCE).await;
    assert_eq!(ring_deadlines(&s), 0, "no ring deadline before the callee's first 18x");
    let rang = Instant::now();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cancel = s.bob.receive("CANCEL").await;
    assert_eq!(
        Instant::now() - rang,
        FIRST_18X_TO_CANCEL,
        "the ring ran no_answer_ms from the 180"
    );
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    call.expect(480).await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A 183 with early media is the leg's first 18x: the deadline runs from it,
/// and the 180 that follows does not restart it.
#[tokio::test(start_paused = true)]
async fn a_183_is_the_first_18x_and_a_later_180_does_not_restart_the_ring() {
    const NAME: &str = "ring-from-183";
    let s = scene(NAME, false).await;
    let mut call = dial(&s, NAME).await;
    let mut uas = s.bob.receive("INVITE").await;
    s.h.advance(SILENCE).await;
    let rang = Instant::now();
    uas.respond(183, "Session Progress").with_sdp(ANSWER).await;
    call.expect(183).await;
    s.h.advance(Duration::from_millis(1_000)).await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cancel = s.bob.receive("CANCEL").await;
    assert_eq!(
        Instant::now() - rang,
        FIRST_18X_TO_CANCEL,
        "the ring ran no_answer_ms from the 183"
    );
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    call.expect(480).await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A forked callee rings on two early dialogs (RFC 3261 §12.1): the leg's
/// deadline runs from the first fork's 180; the second fork's does not move
/// it.
#[tokio::test(start_paused = true)]
async fn a_second_forks_18x_does_not_restart_the_legs_ring() {
    const NAME: &str = "ring-from-first-fork";
    let s = scene(NAME, false).await;
    let mut call = dial(&s, NAME).await;
    let mut uas = s.bob.receive("INVITE").await;
    s.h.advance(SILENCE).await;
    let rang = Instant::now();
    uas.respond(180, "Ringing").with_to_tag("fork1").await;
    call.expect(180).await;
    s.h.advance(Duration::from_millis(900)).await;
    uas.respond(180, "Ringing").with_to_tag("fork2").await;
    call.expect(180).await;

    let mut cancel = s.bob.receive("CANCEL").await;
    assert_eq!(
        Instant::now() - rang,
        FIRST_18X_TO_CANCEL,
        "the ring ran from the first fork's 180"
    );
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").with_to_tag("fork1").await;
    s.bob.receive("ACK").await;
    call.expect(480).await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.alice.drain().await;
    s.bob.drain().await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// The callee answers with a 100 alone, then its 200 well past `no_answer_ms`:
/// a leg that never sent an 18x never rang, so nothing CANCELs it and the call
/// is established.
#[tokio::test(start_paused = true)]
async fn a_leg_answered_before_any_18x_never_arms_the_ring() {
    const NAME: &str = "ring-never-armed-answered";
    let s = scene(NAME, false).await;
    let mut call = dial(&s, NAME).await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(100, "Trying").await;
    s.h.advance(Duration::from_millis(RING_MS + 1_000)).await;
    assert_eq!(ring_deadlines(&s), 0, "a leg with no 18x holds no ring deadline");
    assert!(
        s.bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "no CANCEL without an 18x"
    );

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// The callee refuses with a 486 well past `no_answer_ms`, having sent no
/// 18x: its refusal, not a ring deadline, ends the leg.
#[tokio::test(start_paused = true)]
async fn a_leg_refused_before_any_18x_never_arms_the_ring() {
    const NAME: &str = "ring-never-armed-refused";
    let s = scene(NAME, false).await;
    let mut call = dial(&s, NAME).await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(100, "Trying").await;
    s.h.advance(Duration::from_millis(RING_MS + 1_000)).await;
    assert!(
        s.bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "no CANCEL without an 18x"
    );

    uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    call.expect(486).await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// The first callee refuses at once and the failure consult routes to carol:
/// carol's leg runs its own deadline from its own first 18x.
#[tokio::test(start_paused = true)]
async fn a_failover_leg_runs_its_ring_from_its_own_first_18x() {
    const NAME: &str = "ring-failover-leg";
    let s = scene(NAME, true).await;
    let mut call = dial(&s, NAME).await;
    let mut bob_uas = s.bob.receive("INVITE").await;
    bob_uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;

    let mut carol_uas = s.carol.receive("INVITE").await;
    s.h.advance(SILENCE).await;
    assert_eq!(ring_deadlines(&s), 0, "carol's leg holds no ring deadline before its 18x");
    let rang = Instant::now();
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cancel = s.carol.receive("CANCEL").await;
    assert_eq!(Instant::now() - rang, FIRST_18X_TO_CANCEL, "carol's ring ran from her own 180");
    cancel.respond(200, "OK").await;
    carol_uas.respond(487, "Request Terminated").await;
    s.carol.receive("ACK").await;
    call.expect(480).await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A transfer target (RFC 3515 REFER) rings 700 ms after its INVITE: the
/// transfer's ring deadline runs from its first 18x.
#[tokio::test(start_paused = true)]
async fn a_transfer_targets_ring_runs_from_its_own_first_18x() {
    let h = Harness::with_transit_delay("ring-transfer-target", TRANSIT_MS);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let charlie = h.agent_auto("charlie").await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let allow = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{}}},"no_answer_ms":{RING_MS}}}"#,
        charlie.addr().port()
    );
    let mut refer = bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@{}>", charlie.addr()))
        .with_header("X-Api-Call", &allow)
        .send()
        .await;
    refer.expect(202).await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    h.advance(SILENCE).await;
    let rang = Instant::now();
    charlie_uas.respond(180, "Ringing").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;

    let mut cancel = charlie.receive("CANCEL").await;
    assert_eq!(
        Instant::now() - rang,
        FIRST_18X_TO_CANCEL,
        "the transfer target's ring ran from its own 180"
    );
    cancel.respond(200, "OK").await;
    charlie_uas.respond(487, "Request Terminated").await;
    charlie.receive("ACK").await;
    bob.receive_tolerating("NOTIFY", &["NOTIFY"]).await.respond(200, "OK").await;

    let mut alice_bye = alice_dialog.bye().await;
    bob.receive_tolerating("BYE", &["NOTIFY"]).await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

//! A b-leg whose hop answered nothing, not even a 100, is reported dead at
//! the first-response deadline (ADR-0044) and the failure consult reroutes
//! the call. The dead leg's CANCEL stays with its transaction until a
//! provisional (RFC 3261 §9.1), so the call waits on nothing from it: once
//! the rerouted call ends, it is released at once, with its limiter hold and
//! its CDR, not at the terminating backstop. A hop that answers after all
//! puts that CANCEL on the wire, and the call waits for its INVITE's final.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, ClientInvite, Harness, ServerTxn};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=carol 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The default first-response deadline, and a margin past it.
const DEAD_HOP: Duration = Duration::from_millis(5_300);

/// The longest the release may follow the caller's BYE: the turns that
/// answer it and the limiter release queue, far under the backstop.
const PROMPT: Duration = Duration::from_secs(1);

/// Past the end of the dead hop's INVITE transaction: Timer B, 64·T1 from
/// the send, then the give-up's 64·T1 hold (RFC 3261 §17.1.1.2).
const TXN_OVER: Duration = Duration::from_secs(70);

/// Routes to bob, holding `trunk`, with a callback context; a failure fails
/// over to carol, holding `trunk`.
fn failing_over() -> Arc<dyn CallDecisionEngine> {
    let trunk = || vec![LimiterEntry { id: "trunk".into(), limit: 10 }];
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("dead-hop-ctx".into());
                r.call_limiter = trunk();
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| {
                assert_eq!(req.failure.timeout_kind.as_deref(), Some("response"));
                let mut r = route_to("127.0.0.1", 5071);
                r.call_limiter = trunk();
                CallTreatment::Route(r)
            })
            .build(),
    )
}

/// alice, bob, carol and the B2BUA, failing over from bob to carol.
struct Scene {
    h: Harness,
    alice: Agent,
    bob: Agent,
    carol: Agent,
    b2bua: B2buaSut,
}

impl Scene {
    async fn new(name: &str) -> Scene {
        let h = Harness::new(name);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let carol = h.agent("carol", "127.0.0.1:5071").await;
        let b2bua = B2buaSut::builder(failing_over())
            .tune(|c| c.keepalive_interval_sec = 300)
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;
        Scene { h, alice, bob, carol, b2bua }
    }

    /// alice calls; bob takes the INVITE and answers nothing until the
    /// first-response deadline, which dials carol: alice's INVITE and bob's.
    async fn dead_hop_dials_carol(&self) -> (ClientInvite, ServerTxn) {
        let call = self.alice.invite(&self.bob).with_sdp(OFFER).through(self.b2bua.addr).send();
        let call = call.await;
        let dead = self.bob.receive("INVITE").await;
        self.h.advance(DEAD_HOP).await;
        (call, dead)
    }

    /// carol answers alice's call, which is confirmed, and alice hangs up.
    async fn carol_answers_and_alice_hangs_up(&self, mut call: ClientInvite) {
        let mut uas = self.carol.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        self.carol.receive("ACK").await;
        scenario_harness::callflow::hangup(&mut dialog, &self.carol).await;
    }

    fn released_with_its_cdr(&self) -> bool {
        self.b2bua.is_reaped() && !self.b2bua.cdr_records().is_empty()
    }

    /// The call's one CDR, and every limiter hold, released.
    fn assert_one_cdr_and_every_hold_released(&self) {
        assert_eq!(self.b2bua.cdr_records().len(), 1, "with its one CDR");
        let count = self.b2bua.limiter_count();
        assert!(count.admitted >= 2, "bob's route and carol's held `trunk`");
        assert_eq!(count.released, count.admitted, "every hold released with the call");
        self.b2bua.assert_fully_reaped();
    }

    /// Whether a CANCEL reached bob.
    async fn bob_cancelled(self) -> bool {
        let bob_addr = self.bob.addr();
        let report = self.h.finish().await;
        report.entries().iter().any(|e| e.to == bob_addr && e.raw.starts_with(b"CANCEL "))
    }
}

/// bob answers nothing; carol, dialled on the failure consult, answers;
/// alice hangs up. The call is released, its holds freed and its one CDR
/// written within [`PROMPT`] of her BYE. bob's INVITE transaction runs to its
/// end without a CANCEL ever reaching him.
#[tokio::test(start_paused = true)]
async fn a_rerouted_call_whose_first_hop_was_dead_is_released_when_it_ends() {
    let s = Scene::new("dead-hop-reroute-release").await;
    let (call, _dead) = s.dead_hop_dials_carol().await;

    let hung_up = tokio::time::Instant::now();
    s.carol_answers_and_alice_hangs_up(call).await;
    settle_until(|| s.released_with_its_cdr()).await;
    let released = hung_up.elapsed();
    assert!(s.b2bua.is_reaped(), "the call is released when it ends");
    assert!(released < PROMPT, "released {released:?} after the BYE, not at the backstop");
    s.assert_one_cdr_and_every_hold_released();

    s.h.advance(TXN_OVER).await;
    assert_eq!(s.b2bua.txn_metrics().active_transactions(), 0, "bob's transaction ends");
    assert!(!s.bob_cancelled().await, "no CANCEL to a hop that answered nothing (RFC 3261 §9.1)");
}

/// bob answers nothing past the deadline, then rings after all: the CANCEL
/// held for him leaves on his 180. alice is connected to carol and hangs up
/// while that CANCEL is unanswered: the call waits for bob's INVITE's final
/// (§9.1: a 200 may still cross the CANCEL), and is released on his 487.
#[tokio::test(start_paused = true)]
async fn a_dead_hop_that_rings_after_all_holds_the_release_until_its_final() {
    let s = Scene::new("dead-hop-rings-late").await;
    let (call, mut dead) = s.dead_hop_dials_carol().await;
    dead.respond(180, "Ringing").await;
    let mut cancel = s.bob.receive("CANCEL").await;

    s.carol_answers_and_alice_hangs_up(call).await;
    settle_until(|| s.released_with_its_cdr()).await;
    assert!(!s.b2bua.is_reaped(), "the call waits for the final of bob's CANCELed INVITE");

    cancel.respond(200, "OK").await;
    dead.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    settle_until(|| s.released_with_its_cdr()).await;
    s.assert_one_cdr_and_every_hold_released();
    assert!(s.bob_cancelled().await, "bob's ringing INVITE is CANCELed");
}

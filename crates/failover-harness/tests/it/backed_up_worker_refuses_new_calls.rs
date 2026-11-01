//! **A worker whose event queue is backed up refuses new calls and keeps its
//! dialogs.** The router loop of a call's primary slows to [`PER_TURN`] a turn
//! while a backlog of out-of-dialog requests queues behind it: the proxy's
//! probe still reads the worker alive, and reads it refusing new calls
//! (ADR-0050 §2–§4). A new INVITE the proxy placed on it before its probe read
//! the refusal is refused `503` by the worker's own admission ladder, counted
//! as any ladder refusal, with no call born (ADR-0037). New calls land on the
//! other worker; once the backlog has
//! drained, the held call's re-INVITE and BYE still reach its primary while
//! the proxy reads it refusing, and nothing is taken over. Drained through the
//! hold, the worker reads accepting again.
//!
//! Every call ends with one CDR; the backlog requests are each refused `405`.
//!
//! Beyond the failover sweep: new-call placement around a starved worker, with no node fault.

use std::time::Duration;

use b2bua::admission::Class;
use b2bua::new_calls::Refusal;
use b2bua_harness::{drop_backlog_answers, send_backlog, Pace, BACKLOG_METHOD};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, Acceptance, FailoverHarness, WorkerHealth,
};
use scenario_harness::callflow::OFFER_SDP;
use sip_message::generators::InDialogMethod;
use sip_message::header::RetryAfter;
use tokio::time::Instant;

use crate::departure_support::call_id_placed_on;
use crate::inflight_resend::{REANSWER, REOFFER};
use crate::new_call_acceptance::{
    answered, bring_up, call_ref_of, callee_hangs_up, caller_hangs_up, drain, PROBE_ROUND,
};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const CAROL: &str = "127.0.0.1:5062";

/// The simulated time each turn of the starved worker's router costs.
const PER_TURN: Duration = Duration::from_millis(100);
/// The backlog: fifteen seconds of turns, past one probe round.
const BACKLOG: usize = 150;

/// The other worker's ordinal.
fn other(ordinal: &str) -> &'static str {
    if ordinal == "b1" {
        "b2"
    } else {
        "b1"
    }
}

#[tokio::test(start_paused = true)]
async fn a_backed_up_worker_stays_alive_takes_no_new_call_and_keeps_its_dialogs() {
    let name = "backed-up-worker-refuses-new-calls";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let carol = fh.agent_with_pre_ingress("carol", CAROL, drop_backlog_answers()).await;
    let c = bring_up(&mut fh).await;

    // ── one call answered, then its primary's router starves ────────────────
    let (mut held, mut held_bob, (pri, bak)) = answered(&fh, &alice, &bob, &c.proxy).await;
    let held_ref = call_ref_of(&fh, c.node(&bak), &pri).await;
    let takeovers = c.takeovers();
    fh.mark(&pri, None, "starve", "the primary's router loop slows behind a backlog");
    let pace = fh.turn_points().router_pace().clone();
    pace.set(&pri, Pace::PerTurn(PER_TURN));
    send_backlog(&carol, c.node(&pri).sip_addr(), BACKLOG, "backlog").await;
    let refused = || c.node(&pri).metrics().unroutable_refused_of(BACKLOG_METHOD, 405);

    // ── a new INVITE placed on it before the proxy reads the refusal ────────
    assert_eq!(c.proxy.acceptance(&pri), Some(Acceptance::Accept), "not read refusing yet");
    let mut early = alice
        .invite(&bob)
        .with_sdp(OFFER_SDP)
        .with_call_id(call_id_placed_on(&pri, "early"))
        .through(c.proxy.addr())
        .send()
        .await;

    // ── within a probe round the proxy reads it refusing, and alive ─────────
    let give_up = Instant::now() + PROBE_ROUND + Duration::from_secs(1);
    while Instant::now() < give_up && c.proxy.acceptance(&pri) != Some(Acceptance::Refuse) {
        fh.advance(Duration::from_millis(500)).await;
    }
    assert_eq!(
        c.proxy.acceptance(&pri),
        Some(Acceptance::Refuse),
        "the proxy reads the backed-up worker refusing new calls within a probe round"
    );
    assert_eq!(c.proxy.health(&pri), Some(WorkerHealth::Alive), "the backed-up worker is alive");
    assert!(refused() < BACKLOG as u64, "the backlog is still queued");

    // ── every new call lands on the other worker ────────────────────────────
    let mut new_refs = Vec::new();
    for _ in 0..3 {
        let (mut dialog, _bob_dialog, (new_pri, new_bak)) =
            answered(&fh, &alice, &bob, &c.proxy).await;
        assert_eq!((new_pri.as_str(), new_bak.as_str()), (other(&pri), pri.as_str()));
        new_refs.push(call_ref_of(&fh, c.node(&pri), other(&pri)).await);
        caller_hangs_up(&mut dialog, &bob).await;
    }

    // ── the backlog drains; the held call's re-INVITE and BYE on its primary ─
    let give_up = Instant::now() + Duration::from_secs(20);
    while refused() < BACKLOG as u64 && Instant::now() < give_up {
        fh.advance(Duration::from_millis(500)).await;
    }
    assert_eq!(refused(), BACKLOG as u64, "every backlog request is refused 405");
    let resp = early.expect(503).await;
    assert!(
        resp.header::<RetryAfter>().is_some(),
        "the worker's new-call 503 carries a Retry-After"
    );
    let ladder = c.node(&pri).metrics().new_calls();
    assert_eq!(ladder.rejected(Refusal::EventQueue, Class::Normal), 1, "refused by its ladder");
    let by_any = |reason| ladder.rejected(reason, Class::Normal);
    assert_eq!(Refusal::ALL.into_iter().map(by_any).sum::<u64>(), 1, "and by no other rung");
    pace.set(&pri, Pace::Free);
    assert_eq!(
        c.proxy.acceptance(&pri),
        Some(Acceptance::Refuse),
        "just drained: the proxy still reads the worker refusing"
    );
    let mut round = held.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut at_bob = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    round.expect(200).await;
    held.ack(None).await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    assert!(c.node(&pri).serves(&held_ref), "the refusing worker serves its call");
    assert!(!c.node(&bak).serves(&held_ref), "the backup never served it");
    callee_hangs_up(&mut held_bob, &alice).await;

    // ── drained through the hold, the worker accepts again ──────────────────
    let give_up = Instant::now() + 2 * PROBE_ROUND;
    while Instant::now() < give_up && c.proxy.acceptance(&pri) != Some(Acceptance::Accept) {
        fh.advance(Duration::from_millis(500)).await;
    }
    assert_eq!(
        c.proxy.acceptance(&pri),
        Some(Acceptance::Accept),
        "drained through the hold: the proxy reads the worker accepting"
    );

    drain(&fh, &c).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(c.takeovers(), takeovers, "no call was taken over");
    for call_ref in std::iter::once(&held_ref).chain(&new_refs) {
        assert_eq!(total_cdrs_for(&c.nodes(), call_ref), 1, "one CDR for {call_ref}");
        assert_call_fully_released(&c.nodes(), call_ref).await;
    }
    fh.assert_sip_rfc_clean(name);
}

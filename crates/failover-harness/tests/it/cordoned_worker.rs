//! **A cordoned worker serves its dialogs and takes no new call while another
//! accepts.** An operator cordons a worker (ADR-0050 §9): its OPTIONS answer
//! reports that it refuses new calls, the proxy places every new call on the
//! other, and the cordoned worker's established call keeps its re-INVITE and
//! its BYE, with no takeover. With every worker cordoned a new call still
//! reaches one, which refuses it with its own `503` and writes its record;
//! the proxy refuses nothing itself (§6).
//!
//! ```text
//!   one   b_pri cordoned with one call answered: three new calls land on the
//!         other, the held call's re-INVITE and BYE stay on b_pri
//!   all   both cordoned with one call answered: a new call is refused 503 by
//!         a worker, the held call ends by its BYE
//! ```
//!
//! Every call ends with one CDR, the refused one included.

use std::time::Duration;

use failover_harness::{assert_call_fully_released, total_cdrs_for, FailoverHarness, WorkerHealth};
use sip_message::generators::InDialogMethod;

use crate::inflight_resend::{REANSWER, REOFFER};
use crate::new_call_acceptance::{
    answered, bring_up, call_ref_of, callee_hangs_up, caller_hangs_up, drain, PROBE_ROUND,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The other worker's ordinal.
fn other(ordinal: &str) -> &'static str {
    if ordinal == "b1" {
        "b2"
    } else {
        "b1"
    }
}

#[tokio::test(start_paused = true)]
async fn a_cordoned_worker_takes_no_new_call_while_another_accepts_and_keeps_its_dialogs() {
    let mut fh = FailoverHarness::new("cordoned-worker-keeps-dialogs", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let c = bring_up(&mut fh).await;

    // ── one call answered, then its primary is cordoned ─────────────────────
    let (mut held, mut held_bob, (pri, bak)) = answered(&fh, &alice, &bob, &c.proxy).await;
    let held_ref = call_ref_of(&fh, c.node(&bak), &pri).await;
    fh.mark(&pri, None, "cordon", "the operator cordons the held call's primary");
    c.node(&pri).cordon();
    fh.advance(PROBE_ROUND).await;
    assert_eq!(c.proxy.health(&pri), Some(WorkerHealth::Alive), "the cordoned worker is alive");
    let takeovers = c.takeovers();

    // ── every new call lands on the other worker ────────────────────────────
    let mut new_refs = Vec::new();
    for _ in 0..3 {
        let (mut dialog, _bob_dialog, (new_pri, new_bak)) =
            answered(&fh, &alice, &bob, &c.proxy).await;
        assert_eq!((new_pri.as_str(), new_bak.as_str()), (other(&pri), pri.as_str()));
        new_refs.push(call_ref_of(&fh, c.node(&pri), other(&pri)).await);
        caller_hangs_up(&mut dialog, &bob).await;
    }

    // ── the held call: a re-INVITE and the callee's BYE, on its primary ─────
    let mut round = held.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut at_bob = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    round.expect(200).await;
    held.ack(None).await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    assert!(c.node(&pri).serves(&held_ref), "the cordoned worker serves its call");
    assert!(!c.node(&bak).serves(&held_ref), "the backup never served it");
    callee_hangs_up(&mut held_bob, &alice).await;

    c.node(&pri).uncordon();
    drain(&fh, &c).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(c.takeovers(), takeovers, "no call was taken over");
    for call_ref in std::iter::once(&held_ref).chain(&new_refs) {
        assert_eq!(total_cdrs_for(&c.nodes(), call_ref), 1, "one CDR for {call_ref}");
        assert_call_fully_released(&c.nodes(), call_ref).await;
    }
    fh.assert_sip_rfc_clean("cordoned-worker-keeps-dialogs");
}

#[tokio::test(start_paused = true)]
async fn with_every_worker_cordoned_a_new_call_is_refused_by_a_worker_with_its_record() {
    let mut fh = FailoverHarness::new("every-worker-cordoned", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let c = bring_up(&mut fh).await;

    let (_held, mut held_bob, (pri, bak)) = answered(&fh, &alice, &bob, &c.proxy).await;
    let held_ref = call_ref_of(&fh, c.node(&bak), &pri).await;
    fh.mark("b1", None, "cordon", "the operator cordons every worker");
    c.b1.cordon();
    c.b2.cordon();
    fh.advance(PROBE_ROUND).await;
    let takeovers = c.takeovers();

    // ── a new call reaches a worker, which refuses it ───────────────────────
    let mut refused = alice.invite(&bob).with_sdp(OFFER).through(c.proxy.addr()).send().await;
    refused.expect(503).await;
    fh.advance(Duration::from_secs(1)).await;
    for reason in ["no_target_available", "no_target_selected", "worker_rate_capped"] {
        assert_eq!(
            c.proxy.metrics().reject_count(reason),
            0,
            "the proxy refused nothing: {reason}"
        );
    }

    // ── the held call ends by its BYE ───────────────────────────────────────
    callee_hangs_up(&mut held_bob, &alice).await;

    c.b1.uncordon();
    c.b2.uncordon();
    drain(&fh, &c).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(c.takeovers(), takeovers, "no call was taken over");
    assert_eq!(total_cdrs_for(&c.nodes(), &held_ref), 1, "one CDR for the held call");
    assert_call_fully_released(&c.nodes(), &held_ref).await;
    let cdrs: usize = c.nodes().iter().map(|n| n.cdr_records().len()).sum();
    assert_eq!(cdrs, 2, "one CDR per call, the refused one included");
    fh.assert_sip_rfc_clean("every-worker-cordoned");
}

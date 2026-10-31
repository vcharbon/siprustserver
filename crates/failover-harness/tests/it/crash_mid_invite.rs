//! A crash of the primary mid-INVITE, while the callee rings: the call
//! completes through the backup and leaves its one CDR once the dead primary
//! departs.

use std::time::Duration;

use failover_harness::{worker_ordinals, FailoverHarness, WorkerHealth};

use crate::departure_support::spawn_reactive_proxy;
use crate::failover::find_backed_up_ref;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// Matrix: crash mid-INVITE — the primary crashes while the callee rings,
/// before anyone answers. The callee's 200 fails over to the backup, which
/// takes the call over and relays it; the caller ACKs and later hangs up on
/// the backup. The crashed primary then departs, and the departure sweep
/// writes the call's one CDR (ADR-0047 D8). A fresh call still establishes on
/// the surviving worker.
#[tokio::test(start_paused = true)]
async fn matrix_crash_mid_invite() {
    let mut fh = FailoverHarness::new("s10b-crash-mid-invite", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = spawn_reactive_proxy(&fh).await;
    let mut w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let mut w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;

    // alice INVITEs; bob gets the b-leg. Determine the primary from the cookie.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    let survivor_ord = if pri_ord == "b1" { "b2" } else { "b1" };
    let call_ref = {
        let survivor = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
        find_backed_up_ref(survivor, &pri_ord).await
    };

    fh.mark(&pri_ord, None, "crash", "mid-INVITE (before answer)");
    if pri_ord == "b1" {
        w_b1.crash();
    } else {
        w_b2.crash();
    }
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // The callee answers: the 200 reaches the backup, which relays it.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;

    // The crashed primary leaves the set: the sweep discharges the ended call.
    fh.depart(&pri_ord);
    let survivor = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    let released = fh
        .settle_terminal(async || {
            survivor.memory_clean() && !survivor.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(released, "the ended call is deleted on {survivor_ord}");
    assert_eq!(
        failover_harness::total_cdrs_for(&[survivor], &call_ref),
        1,
        "the call's one CDR, written by the backup at the departure",
    );
    failover_harness::assert_call_fully_released(&[survivor], &call_ref).await;

    // The surviving worker and the proxy still serve a fresh call end to end.
    let mut call2 = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas2 = bob.receive("INVITE").await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call2.expect(200).await;
    let mut dlg2 = call2.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dlg2, &bob).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    let survivor = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    let fresh: Vec<_> =
        survivor.cdr_records().into_iter().filter(|r| r.call_ref != call_ref).collect();
    assert_eq!(fresh.len(), 1, "the fresh call's one CDR");
    failover_harness::assert_call_fully_released(&[survivor], &fresh[0].call_ref).await;
    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

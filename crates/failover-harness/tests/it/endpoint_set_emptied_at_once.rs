//! **An endpoint set emptied at once withdraws no worker (ADR-0031 D6).**
//!
//! The orchestrator deletes and recreates the Service: its endpoint set
//! vanishes, every worker's own endpoint included, then comes back with the
//! same processes behind it. No member was singled out, so no worker reads
//! its own withdrawal: none latches Draining, the withdrawn-but-running gauge
//! stays off, each keeps serving what it holds, and once the set is listed
//! again the call ends on its primary as if nothing happened.
//!
//! ```text
//!   established · replicated · both workers routable
//!   t0          the slice empties: the proxy and both workers' views are
//!               reconciled to the empty set, the processes are untouched
//!   t0..t0+6 s  the empty view outlives a periodic membership reconcile (5 s)
//!   t0+6 s      the slice fills again: the same incarnations, the same addresses
//!   alice ──BYE──▶ proxy ──▶ primary ──▶ 200
//! ```
//!
//! One reconcile is one stored snapshot (`topology::reconcile_to_desired`), so
//! every worker reads the empty set and never a partial one, whatever the
//! runtime.
//!
//! Beyond the failover sweep: a whole-set removal is a control-plane event, not
//! a withdrawal.

use std::time::Duration;

use failover_harness::{worker_ordinals, Belief, FailoverHarness, ProxySut, WorkerHealth};

use crate::inflight_resend::{bring_up, settle_to_one_cdr, survivor_of};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// How long the slice stays empty: past one periodic membership reconcile.
const EMPTY_FOR: Duration = Duration::from_secs(6);

/// The recreated Service lists every endpoint again, and the proxy's probe
/// reads each worker alive.
async fn list_the_slice_again(fh: &mut FailoverHarness, proxy: &ProxySut) {
    fh.fill_the_slice();
    for ord in ["b1", "b2"] {
        proxy.set_health(ord, WorkerHealth::Alive);
    }
    fh.advance(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn an_endpoint_set_emptied_at_once_withdraws_no_worker() {
    let mut fh = FailoverHarness::new("endpoint-set-emptied-at-once", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, w_b1, w_b2) = bring_up(&mut fh).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let primary = if pri_ord == "b1" { &w_b1 } else { &w_b2 };
    let backup = survivor_of(&pri_ord, &w_b1, &w_b2);
    let call_ref =
        backup.scan_one_backed_up(&pri_ord).await.expect("the call replicated to the backup");
    assert!(primary.serves(&call_ref), "the primary serves the call");

    fh.empty_the_slice();
    fh.advance(EMPTY_FOR).await;
    for w in [&w_b1, &w_b2] {
        let ord = w.ordinal();
        assert!(!w.is_withdrawn(), "{ord} read a control-plane event as its own withdrawal");
        assert!(!w.is_draining(), "{ord} latched Draining on an emptied slice");
        assert!(!w.metrics().withdrawn_running(), "{ord} is not withdrawn-but-running");
    }
    assert!(primary.serves(&call_ref), "the primary keeps serving the call");
    assert!(!backup.serves(&call_ref), "the backup took nothing over from a running primary");

    list_the_slice_again(&mut fh, &proxy).await;
    for w in [&w_b1, &w_b2] {
        let ord = w.ordinal();
        assert!(w.is_ready(), "{ord} is ready once the slice lists it again");
        assert!(!w.is_withdrawn() && !w.is_draining(), "{ord} never withdrew");
    }

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;

    let ledger = fh.view_ledger();
    for (ord, peer) in [("b1", "b2"), ("b2", "b1")] {
        let own_view = ledger.beliefs(&format!("{ord}#g1"), ord);
        assert_eq!(
            own_view,
            vec![Belief::Running { gen: 1 }],
            "{ord} ran throughout: {own_view:?}",
        );
        let peer_view = ledger.beliefs(&format!("{ord}#g1"), peer);
        assert!(
            !peer_view.contains(&Belief::PeerParked),
            "{ord} never parked {peer}: {peer_view:?}",
        );
    }

    drop((w_b1, w_b2, proxy));
}

/// The slice empties while the callee rings: the call has its INVITE
/// transaction to serve, the work a departure sweep takes over at once. No
/// peer departed, so the backup takes nothing over and discharges nothing;
/// the callee answers the primary once the slice is listed again, and the
/// call ends there with one CDR.
///
/// ```text
///   alice ─INVITE─▶ proxy ─▶ primary ─INVITE─▶ bob ─180─▶ … (replicated)
///   t0          the slice empties; t0+6 s it fills again
///   bob ─200─▶ primary ─200─▶ alice ─ACK─▶ … BYE
/// ```
#[tokio::test(start_paused = true)]
async fn an_endpoint_set_emptied_while_the_callee_rings_takes_nothing_over() {
    let name = "endpoint-set-emptied-while-ringing";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, w_b1, w_b2) = bring_up(&mut fh).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;

    let primary = if pri_ord == "b1" { &w_b1 } else { &w_b2 };
    let backup = survivor_of(&pri_ord, &w_b1, &w_b2);
    let call_ref =
        backup.scan_one_backed_up(&pri_ord).await.expect("the setup replicated to the backup");
    assert!(primary.serves(&call_ref), "the primary serves the setup");
    let hydrated_before = backup.metrics().repl_takeover_hydrated_total();

    fh.empty_the_slice();
    fh.advance(EMPTY_FOR).await;
    assert_eq!(
        backup.metrics().repl_takeover_hydrated_total(),
        hydrated_before,
        "the backup hydrated nothing: no peer departed",
    );
    assert!(!backup.serves(&call_ref), "the backup took nothing over from a running primary");
    assert!(primary.serves(&call_ref), "the primary keeps serving the setup");
    for w in [&w_b1, &w_b2] {
        let ord = w.ordinal();
        assert!(!w.is_withdrawn() && !w.is_draining(), "{ord} never withdrew");
    }

    list_the_slice_again(&mut fh, &proxy).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(!backup.serves(&call_ref), "the answer reached the primary alone");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);

    drop((w_b1, w_b2, proxy));
}

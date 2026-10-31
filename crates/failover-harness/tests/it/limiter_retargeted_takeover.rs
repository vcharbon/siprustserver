//! A retargeted call's limiter holds across a takeover: the backup releases
//! the call by its limiter key, which frees the set the failover route holds
//! and never the replaced route's ids again.
//!
//! Beyond the failover sweep: the holds of a call retargeted before its primary crashed.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use call::LimiterEntry;
use failover_harness::{assert_call_ended, cookie_field, ReplicatedB2buaSut, WorkerHealth};

use crate::limiter_release_by_call::{spawn_workers, LimiterRig};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
/// The failover target.
const CHARLIE: &str = "127.0.0.1:5072";

/// A failover decision through the worker's outbound proxy: the initial route
/// toward bob holds `x` + `y`; bob's failure fails over to charlie with `y` +
/// `z` (an overlapping set).
fn failover_decision() -> Arc<dyn CallDecisionEngine> {
    let limiters = |ids: &[&str]| -> Vec<LimiterEntry> {
        ids.iter().map(|id| LimiterEntry { id: (*id).into(), limit: 10 }).collect()
    };
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.new_ruri = None;
                r.callback_context = Some("limiter-ha-failover-ctx".into());
                r.call_limiter = limiters(&["x", "y"]);
                NewCallResponse::Route(r)
            })
            .on_failure(move |_req| {
                let mut r = route_to("127.0.0.1", 5072);
                r.new_ruri = Some(format!("sip:{CHARLIE}"));
                r.call_limiter = limiters(&["y", "z"]);
                CallTreatment::Route(r)
            })
            .build(),
    )
}

/// A retargeted call's holds after a takeover. The initial route holds `x` + `y`;
/// bob busies out and the failover route to charlie replaces them with `y` +
/// `z`, which the backup counts in its replica. The primary crashes after the
/// failover and the proxy reads it dead; the caller's BYE fails over to the
/// backup, which releases the call by its limiter key at its end: `y` and `z`
/// are freed, never the replaced `x` again. Each id carries one witness hold
/// admitted under a call of its own, so a surplus release reads below the
/// witness instead of vanishing under the store's floor at 0.
#[tokio::test(start_paused = true)]
async fn retargeted_call_holds_drain_on_the_takeover_node_without_re_releasing_the_replaced_route()
{
    let mut fh =
        failover_harness::FailoverHarness::new("limiter-ha-retargeted-takeover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let charlie = fh.agent("charlie", CHARLIE).await;
    let rig = LimiterRig::serve().await;
    let (proxy, mut w_b1, mut w_b2) = spawn_workers(&mut fh, &rig, failover_decision()).await;

    // ── bob busies out; the failover route to charlie is applied ─────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    let primary_ord = cookie_field(bob_uas.request(), "w_pri").unwrap_or_default();
    assert_eq!(rig.holds(), [1, 1, 0], "the initial route holds x and y");
    bob_uas.respond(486, "Busy Here").await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;

    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    charlie.receive("ACK").await;

    // Drive the answer + replicate primary → backup.
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [0, 1, 1], "the failover route's holds replace the initial route's");

    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let call_ref = backup.scan_one_backed_up(&primary_ord).await.expect("the call replicated");

    // ── the primary crashes; the BYE fails over to the backup ────────────────
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    let creations_before = backup.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog, &charlie).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        backup.metrics().creations_total() > creations_before,
        "backup processed the failed-over BYE",
    );
    assert_eq!(
        rig.holds(),
        [0, 0, 0],
        "the takeover node released the retargeted call's holds at the end: its primary is read dead",
    );
    let reaped = fh.settle_lossy_cleanup(async || !backup.holds_any_trace(&call_ref).await).await;
    assert!(reaped, "the takeover node's reap freed the deferred body");
    assert_eq!(rig.holds(), [0, 0, 0], "each hold released exactly once, the witnesses intact");
    // StayDead: the primary never reclaims, so no CDR — the accepted loss.
    assert_call_ended(&[&w_b1, &w_b2], &call_ref, 0, "StayDead: the accepted loss").await;
    assert_eq!(
        w_b1.metrics().repl_terminal_lost_total() + w_b2.metrics().repl_terminal_lost_total(),
        1,
        "exactly one lost-CDR cleanup counted across the cluster",
    );
}

//! **The caller's copy of her unanswered INVITE takes the call over.** The
//! node serving a routed call dies after its write reached the backup and
//! before anything it sent reached a party: the caller heard no 100, so she
//! retransmits (RFC 3261 §17.1.1.2), and the callee never got the INVITE. The
//! peers are not told of the death (ADR-0031 case 3a), so no departure sweep
//! runs. The front proxy reads the node dead and places her copy on the
//! backup, which holds the call's record and serves it: the copy is absorbed
//! by the caller's INVITE transaction the record rebuilds, and the dialled
//! INVITE is sent again under its own branch.
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ b1 ─INVITE─✗ bob     (alice and bob cut off)
//!                            ✗ b1 killed, unannounced ✗
//!   alice ─INVITE (copy)─▶ proxy ─▶ b2 ─INVITE─▶ bob   (b2 takes the call over)
//!   alice ◀─180/200── b2 ◀─180/200── bob; ACK, BYE
//! ```

use failover_harness::FailoverHarness;

use crate::departure_support::placed;
use crate::inflight_resend::{
    bring_up, kill_serving_node, reboot_and_reclaim, settle_to_one_cdr, split_mut, survivor_of,
    REPLICATE,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

#[tokio::test(start_paused = true)]
async fn the_callers_copy_of_a_routed_unanswered_invite_takes_the_call_over() {
    let name = "caller-copy-takes-over";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    fh.cut_signalling("bob", bob.addr());
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    fh.cut_signalling("alice", alice.addr());
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.restore_signalling("bob", bob.addr());
    fh.restore_signalling("alice", alice.addr());

    call.retransmit().await;
    let mut uas = bob.receive("INVITE").await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated + 1,
        "the caller's copy took the call over on the backup",
    );
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

//! **The caller's copy of an answered INVITE holds the takeover copy until
//! its Timer L.** The primary relays the callee's 2xx and replicates the
//! answered call, but the 2xx never reaches the caller, who heard no 100
//! either; the primary then dies unannounced (ADR-0031 case 3a). The
//! caller's retransmission of the INVITE (RFC 3261 §17.1.1.2) reaches the
//! backup, whose transaction layer opens a server transaction for it,
//! attributed to no call as every initial INVITE is. The backup takes the
//! call over from its record: that transaction takes the record's 2xx (RFC
//! 6026 §7.1) and answers the caller. It is the copy's transaction from then
//! on, so the copy stays resident while it lives, until its Timer L, and is
//! shed once it clears (ADR-0014).
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ b1 ─INVITE─▶ bob ─200─▶ b1 ─200─╳ alice
//!                            ✗ b1 killed, unannounced ✗
//!   alice ─INVITE (copy)─▶ proxy ─▶ b2 (takes over) ─200─▶ alice; ACK ─▶ bob
//!   b2 holds the copy until the INVITE transaction's Timer L, then sheds it
//! ```
//!
//! Beyond the failover sweep: an INVITE copy taking an answered seed's final.

use std::time::Duration;

use failover_harness::FailoverHarness;

use crate::departure_support::{bring_up_reactive, placed};
use crate::inflight_resend::{
    kill_serving_node, reboot_and_reclaim, settle_to_one_cdr, split_mut, survivor_of, REPLICATE, T1,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Timer L (RFC 6026 §8.7): 64·T1.
const TIMER_L: Duration = Duration::from_millis(64 * 500);

#[tokio::test(start_paused = true)]
async fn the_callers_copy_of_an_answered_invite_holds_the_takeover_copy_until_its_timer_l() {
    let name = "answered-invite-copy-holds-the-takeover";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let (proxy, mut w_b1, mut w_b2) = bring_up_reactive(&mut fh).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    fh.cut_signalling("alice", alice.addr());
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    fh.restore_signalling("alice", alice.addr());

    call.retransmit().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated + 1,
        "the caller's copy took the call over on the backup",
    );

    fh.advance(T1).await;
    assert!(
        survivor_of(&pri_ord, &w_b1, &w_b2).serves(&call_ref),
        "the INVITE transaction the caller's copy opened holds the takeover copy",
    );
    let mut held = T1;
    while survivor_of(&pri_ord, &w_b1, &w_b2).serves(&call_ref) && held < TIMER_L * 2 {
        fh.advance(T1).await;
        held += T1;
    }
    assert!(
        !survivor_of(&pri_ord, &w_b1, &w_b2).serves(&call_ref),
        "the copy is shed once the INVITE transaction's Timer L clears it, held {held:?}",
    );

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

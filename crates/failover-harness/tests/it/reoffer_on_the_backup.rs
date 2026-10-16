//! **A re-offer on a refused call's identity is a new call on the backup.**
//! A call routed by its primary P ends in the callee's 404, relayed to the
//! caller. P then refuses new calls, so the proxy places the caller's
//! re-offer — same Call-ID and From-tag, CSeq + 1, a new branch (RFC 3261
//! §8.1.3.5) — on P's backup S while S still holds the ended call's replica.
//! S places it as the new call it is, with its own CDR; it takes nothing
//! over from the live P.

use std::time::Duration;

use failover_harness::{worker_ordinals, Acceptance, FailoverHarness};

use crate::inflight_resend::bring_up;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const CALL_ID: &str = "reoffer-1@127.0.0.1";
const FROM_TAG: &str = "reoffer-tag";

#[tokio::test(start_paused = true)]
async fn a_reoffer_placed_on_the_backup_is_a_new_call_there() {
    let name = "reoffer-on-the-backup";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let (proxy, w_b1, w_b2) = bring_up(&mut fh).await;

    // ── the first call rings, replicates, and ends in the callee's 404 ──────
    let invite = alice.invite(&bob).with_sdp(OFFER).identity(CALL_ID, FROM_TAG);
    let mut first = invite.through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri, bak) = worker_ordinals(uas.request());
    uas.respond(180, "Ringing").await;
    first.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let backup = if bak == "b1" { &w_b1 } else { &w_b2 };
    assert!(backup.scan_one_backed_up(&pri).await.is_some(), "the call replicated");
    let hydrated = backup.metrics().repl_takeover_hydrated_total();
    uas.respond(404, "Not Found").await;
    first.expect(404).await;

    // ── the primary refuses; the re-offer lands on the backup ───────────────
    proxy.set_acceptance(&pri, Acceptance::Refuse);
    let reoffer = alice.invite(&bob).with_sdp(OFFER).identity(CALL_ID, FROM_TAG);
    let mut second = reoffer.cseq(first.invite_cseq() + 1).through(proxy.addr()).send().await;
    let mut uas = bob.receive_tolerating("INVITE", &["ACK"]).await;
    assert_eq!(worker_ordinals(uas.request()).0, bak, "placed on the backup");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    second.expect(200).await;
    let mut dialog = second.ack().await;
    bob.receive_tolerating("ACK", &["ACK"]).await;
    assert_eq!(
        backup.metrics().repl_takeover_hydrated_total(),
        hydrated,
        "the backup took nothing over"
    );
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    let drained = fh.settle_terminal(async || w_b1.memory_clean() && w_b2.memory_clean()).await;
    assert!(drained, "the cluster drains both calls");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    let cdrs: usize = [&w_b1, &w_b2].iter().map(|n| n.cdr_records().len()).sum();
    assert_eq!(cdrs, 2, "one CDR per call");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

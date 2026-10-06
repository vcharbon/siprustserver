//! **A retried INVITE on a challenged call's identity is replicated like any
//! other call.** A caller that answers a 401 re-sends the INVITE with the same
//! Call-ID and From tag at CSeq+1 (RFC 3261 §22.2), so the retry is born on the
//! callRef the challenged call was just deleted under. The delete's
//! resurrection guard buries that call, not the callRef: the retry's writes
//! land in the primary's store and on its backup at once, and a takeover right
//! after keeps the call.
//!
//! ```text
//!   INVITE → 18x… → 401 → ACK     the challenged call ends on its primary
//!   INVITE (CSeq 2) → 200 → ACK   the retry, same Call-ID / From tag
//!                                 its body is stored and replicated
//!   primary crashes               the proxy routes in-dialog traffic to the backup
//!   alice re-INVITE → 200 → ACK   served by the backup's takeover copy
//!   alice BYE                     served by the backup, deferred terminal
//!   primary reboots               reclaims the deferred terminal: one CDR per call
//! ```
//!
//! Timing is the axis ([`Retry`]): the retry either reaches the primary after
//! the challenged call's delete has replicated, or inside the same replication
//! poll, so the backup never sees that delete — the primary's changelog keeps
//! one entry per callRef — and holds the challenged call, at a higher `(p,b)`
//! than the retry's first versions, when the retry's `Put` lands.

use std::time::Duration;

use call::{Call, CallBodyCodec, MsgpackCodec};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, PartitionRole,
    ProxySut, ReplicatedB2buaSut, WorkerHealth,
};
use repl_net::frame::{Frame, Op};
use repl_net::transport::Direction;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const CALL_ID: &str = "challenged-call@127.0.0.1";
const FROM_TAG: &str = "challenged-from-tag";

/// When the caller's retry reaches the primary, relative to the replication
/// of the challenged call's delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retry {
    /// Half a second after the challenge: the delete has reached the backup.
    AfterTheDeleteReplicated,
    /// At once: the delete and the retry's first write share one changelog
    /// entry, which drains as the retry's `Put`.
    InsideOnePoll,
}

/// Decode a stored body.
fn decode(body: &[u8]) -> Call {
    MsgpackCodec::new().decode(body).expect("the stored body decodes")
}

/// Reboot the crashed primary EMPTY at a higher gen + new pod IP, re-learn its
/// address, drive it ready and let the go-active reclaim run.
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    backup: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &ProxySut,
) {
    fh.mark(primary_ord, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = primary.reboot().await;
    proxy.set_address(primary_ord, new_addr);
    fh.note_worker_rebound(primary_ord, new_addr);
    backup.simulate_peer_added(primary_ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {primary_ord} became ready");
    proxy.set_health(primary_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;
}

/// Whether a `Delete` of `call_ref` was received anywhere on the repl fabric.
fn delete_received(fh: &FailoverHarness, call_ref: &str) -> bool {
    fh.repl_report().frames.iter().any(|f| {
        f.dir == Direction::Received
            && matches!(&f.frame, Frame::Data { op: Op::Delete, call_ref: r, .. } if r == call_ref)
    })
}

async fn a_retry_on_a_challenged_identity(name: &str, retry: Retry) {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let mut w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── the first call rings, replicates, then is challenged ────────────────
    let mut first = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(proxy.addr())
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let (primary_ord, _) = worker_ordinals(uas.request());
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let call_ref = call::derive_call_ref(&primary_ord, CALL_ID, FROM_TAG);
    for (code, reason) in [(180, "Ringing"), (181, "Call Is Being Forwarded"), (183, "Progress")] {
        uas.respond(code, reason).await;
        first.expect(code).await;
    }
    fh.advance(Duration::from_millis(500)).await;
    let challenged = backup
        .get(PartitionRole::Backup, &primary_ord, &call_ref)
        .await
        .expect("the challenged call is replicated to the backup");
    let challenged = decode(&challenged);
    let challenged_p = challenged.topology.as_ref().expect("a replicated call has topology").gen;

    uas.respond(401, "Unauthorized")
        .with_header("WWW-Authenticate", "Digest realm=\"bob\", nonce=\"n1\"")
        .await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    first.expect(401).await;
    if retry == Retry::AfterTheDeleteReplicated {
        fh.advance(Duration::from_millis(500)).await;
        assert!(delete_received(&fh, &call_ref), "the challenged call's delete replicated");
    }
    assert!(!primary.serves(&call_ref), "the challenged call is released on its primary");

    // ── the retry on the same identity is answered ──────────────────────────
    let mut retry_call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .cseq(2)
        .with_sdp(OFFER)
        .through(proxy.addr())
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(
        worker_ordinals(uas.request()).0,
        primary_ord,
        "the retry lands on the same primary"
    );
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    retry_call.expect(200).await;
    let mut dialog = retry_call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let served = primary.live_call(&call_ref).expect("the primary serves the retry");
    assert_ne!(served.incarnation(), challenged.incarnation(), "the retry is a call of its own");
    if retry == Retry::InsideOnePoll {
        // This timing is what the scenario is for: the backup held the
        // challenged call when the retry's Put landed, at a version the retry's
        // write does not dominate.
        assert!(!delete_received(&fh, &call_ref), "the delete and the retry's Put compacted");
        let p = served.topology.as_ref().expect("the retry has topology").gen;
        assert!(p <= challenged_p, "the retry's version ({p}) dominates the challenged one's");
    }
    let stored = primary
        .get(PartitionRole::Primary, &primary_ord, &call_ref)
        .await
        .expect("the retry's body is in the primary's store");
    assert_eq!(decode(&stored).incarnation(), served.incarnation(), "the primary stores the retry");
    let replica = backup
        .get(PartitionRole::Backup, &primary_ord, &call_ref)
        .await
        .expect("the retry's body is replicated to the backup");
    let replica = decode(&replica);
    assert_eq!(replica.incarnation(), served.incarnation(), "the backup holds the retry");
    assert!(call::helpers::caller_answered(&replica), "the backup holds the answered retry");

    // ── the primary dies; the backup serves the re-INVITE ───────────────────
    fh.mark(&primary_ord, None, "crash", "primary down");
    primary.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_millis(300)).await;

    let mut reinv = dialog.request(InDialogMethod::Invite, None).await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await;
    dialog.ack(Some(ANSWER)).await;
    bob.receive("ACK").await;
    let taken = backup.live_call(&call_ref).expect("the backup serves the taken-over retry");
    assert_eq!(taken.incarnation(), served.incarnation(), "the takeover copy is the retry");

    // ── the call ends on the backup; the rebooted primary discharges it ─────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;

    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(
        total_cdrs_for(&[&w_b1, &w_b2], &call_ref),
        2,
        "one CDR for the challenged call and one for the retry"
    );
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_retry_after_the_challenge_replicated_survives_a_takeover() {
    a_retry_on_a_challenged_identity(
        "retry-after-the-challenge-replicated",
        Retry::AfterTheDeleteReplicated,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_retry_inside_one_poll_of_the_challenge_survives_a_takeover() {
    a_retry_on_a_challenged_identity(
        "retry-inside-one-poll-of-the-challenge",
        Retry::InsideOnePoll,
    )
    .await;
}

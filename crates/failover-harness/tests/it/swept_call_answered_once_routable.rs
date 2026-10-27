//! **A swept ringing call answered at its returned node once it is routable.**
//! The ringing call's node crashes and its backup's departure sweep takes the
//! call over (ADR-0047). The node restarts, recovers its record from the
//! backup and parks the call it reclaims, writing it once (D7), while its own
//! endpoint does not read ready (ADR-0055 §8). Its writes reach the backup
//! late, as a reclaim burst over a loaded partition does. The front proxy then
//! publishes the node `Unknown`: the callee's 200, its next Via naming the
//! node, reaches the node, while the caller's in-dialog ACK still goes to the
//! backup (ADR-0050 §4).
//!
//! ```text
//!   b1 crashes; b2's sweep takes the ringing call over
//!   b1 restarts, recovers from b2, reads ready itself, its endpoint not;
//!                    its writes reach b2 300 ms late
//!   parked           b1 parks the call and writes it once, sending nothing;
//!                    b2's copy stands down on that write
//!   b1 routable      its endpoint reads ready, the proxy publishes it Unknown
//!   answer           the callee answers at b1; the caller ACKs to b2
//! ```
//!
//! - The burst drained before the node is routable: the caller gets the 200
//!   alone, the callee its ACK, and the call lives on to the caller's BYE.
//! - The answer's write still late when the caller's ACK reaches the backup,
//!   past the 100 ms hold cap: the backup reads that ACK as evidence of a lost
//!   write and ends the call (§4, trigger 3) from the ringing copy it holds —
//!   the caller BYEd, the callee CANCELled. The returned node meets that
//!   ending holding the callee's dialog confirmed, so it ends that dialog
//!   itself: the 2xx ACKed and the dialog BYEd (RFC 3261 §13.2.2.4, §15), and
//!   its 200 to the caller no longer repeated.
//! - The same, the caller's INVITE offerless: the callee's 2xx makes the offer
//!   and the caller's ACK, gone to the backup, carries the answer. The node
//!   ACKs the 2xx answering every stream rejected (RFC 3264 §6), then BYEs.

use std::time::Duration;

use call::CdrEventType;
use failover_harness::{total_cdrs_for, WorkerHealth};

use crate::departure_support::{B1, B2};
use crate::ringing_call_taken_over_on_departure::{
    depart, place, place_offerless, Callee, Placed, ANSWER, OFFER,
};
use crate::swept_call_primary_returns::{answer, hang_up_and_settle, port, reboot, sent};

/// How late the returned node's writes reach the backup: past the 100 ms
/// replication hold cap.
const LAG_MS: u64 = 300;

/// The node restarts with its writes `LAG_MS` late, recovers its record from
/// the backup and becomes ready itself, its endpoint not: it parks the call,
/// sends nothing, and the backup's copy stands down on its reclaim write.
async fn return_parked(p: &mut Placed) {
    p.fh.delay("b1", "b2", LAG_MS);
    let before = from_b1(p);
    reboot(p).await;
    p.fh.flap_not_ready("b1");
    p.w_b2.simulate_peer_added("b1");
    let ready = p
        .fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(10), async || p.w_b1.is_ready())
        .await;
    assert!(ready, "the returning node became ready");
    let stood_down =
        p.fh.pump_until(Duration::from_millis(100), Duration::from_secs(5), async || {
            !p.w_b2.serves(&p.ringing_ref)
        })
        .await;
    assert!(stood_down, "the takeover copy stands down before its node is routable");
    assert!(p.w_b1.parks(&p.ringing_ref), "the returned node parks the call, unarmed");
    assert_eq!(from_b1(p), before, "the parked node sends nothing before it is routable");
}

/// The workers that sent a message starting with `prefix` toward `to`, each
/// once in wire order: a request repeated until its final counts once.
fn senders(p: &Placed, to: std::net::SocketAddr, prefix: &[u8]) -> Vec<u16> {
    let mut senders = sent(p, to, prefix);
    senders.dedup();
    senders
}

/// How many ACKs the node `b1` sent, every incarnation alike.
fn acks_from_b1(p: &Placed) -> usize {
    sent(p, p.bob.addr(), b"ACK sip:bob@").iter().filter(|&&from| from == port(B1)).count()
}

/// How many SIP messages the node `b1` sent, every incarnation alike.
fn from_b1(p: &Placed) -> usize {
    p.fh.sip_entries().iter().filter(|e| e.from.port() == port(B1)).count()
}

/// The node's endpoint reads ready and the proxy publishes it `Unknown`.
fn routable(p: &mut Placed) {
    p.fh.flap_ready("b1");
    p.proxy.set_health("b1", WorkerHealth::Unknown);
}

/// The burst of reclaim writes drained before the node turned routable: the
/// callee answers at once at the node, the caller's ACK reaches the backup,
/// which holds the answer; the call lives on to the caller's BYE.
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_as_its_returned_node_turns_routable_stays_up() {
    let mut p = place("swept-call-answered-as-node-turns-routable", Callee::Ringing).await;
    depart(&mut p).await;
    return_parked(&mut p).await;
    p.fh.heal("b1", "b2");
    routable(&mut p);

    let answered = answer(&mut p).await;
    p.fh.advance(Duration::from_secs(2)).await;
    assert_eq!(
        sent(&p, p.alice.addr(), b"SIP/2.0 200").last(),
        Some(&port(B1)),
        "the returned node relays the callee's 200"
    );
    assert!(sent(&p, p.bob.addr(), b"CANCEL").is_empty(), "no copy CANCELs the answered callee");
    assert!(sent(&p, p.alice.addr(), b"BYE").is_empty(), "no copy ends the answered call");
    assert!(sent(&p, p.bob.addr(), b"BYE").is_empty(), "no copy BYEs the answered callee");
    p.proxy.set_health("b1", WorkerHealth::Alive);
    hang_up_and_settle(p, answered, &[]).await;
}

/// The answer's write still reaches the backup past the hold cap when the
/// caller's ACK does: the backup ends the call from its ringing copy, and the
/// returned node ends the callee's dialog it holds confirmed — the 2xx ACKed,
/// the dialog BYEd — and stops repeating its 200 to the caller.
#[tokio::test(start_paused = true)]
async fn a_swept_call_answered_with_its_answer_write_late_ends_every_dialog_cleanly() {
    let mut p = place("swept-call-answered-answer-write-late", Callee::Ringing).await;
    depart(&mut p).await;
    return_parked(&mut p).await;
    routable(&mut p);

    let acks = acks_from_b1(&p);
    p.at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    p.ringing.expect(200).await;
    let _answered = p.ringing.ack().await;
    p.fh.advance(Duration::from_secs(1)).await;
    let bye = p.alice.try_receive_tolerating("BYE", &[]).await;
    bye.expect("the caller is BYEd").respond(200, "OK").await;
    let cancel = p.bob.try_receive_tolerating("CANCEL", &[]).await;
    cancel
        .expect("the backup CANCELs the INVITE its copy holds unanswered")
        .respond(200, "OK")
        .await;
    let ack = p.bob.try_receive_tolerating("ACK", &["CANCEL"]).await;
    assert!(ack.is_some(), "the callee's 2xx is ACKed at once");
    let bye = p.bob.try_receive_tolerating("BYE", &["CANCEL"]).await;
    bye.expect("the callee is BYEd at once").respond(200, "OK").await;
    let b1 = port(B1);
    assert_eq!(
        senders(&p, p.alice.addr(), b"BYE sip:alice@"),
        [port(B2)],
        "the backup BYEs the caller"
    );
    assert_eq!(acks_from_b1(&p), acks + 1, "the returned node ACKs the callee's 2xx once");
    assert_eq!(senders(&p, p.bob.addr(), b"BYE sip:bob@"), [b1], "the node BYEs the callee");

    let repeated = sent(&p, p.alice.addr(), b"SIP/2.0 200").len();
    p.fh.advance(Duration::from_secs(10)).await;
    assert_eq!(
        sent(&p, p.alice.addr(), b"SIP/2.0 200").len(),
        repeated,
        "the 200 to the caller is not repeated once the call ended"
    );
    let written =
        p.fh.pump_until(Duration::from_millis(500), Duration::from_secs(60), async || {
            total_cdrs_for(&[&p.w_b1, &p.w_b2], &p.ringing_ref) > 0
        })
        .await;
    assert!(written, "the ended call has its CDR");
    let records = [&p.w_b1, &p.w_b2].map(|w| w.cdr_records());
    let cdrs = records.iter().flatten().filter(|r| r.call_ref == p.ringing_ref);
    for cdr in cdrs {
        let answered =
            cdr.events.iter().any(|e| e.event_type == CdrEventType::Answer && e.reason.is_none());
        assert!(answered, "the CDR records the call answered: {:?}", cdr.events);
    }
    p.proxy.set_health("b1", WorkerHealth::Alive);
    crate::swept_call_primary_returns::hang_up_quiet_and_settle(p, &[]).await;
}

/// The late answer write with the caller's INVITE offerless: the callee's 200
/// offers, the caller's ACK carrying the answer goes to the backup. The node
/// meeting the backup's ending ACKs the 2xx with every stream rejected, then
/// BYEs the callee, both at once rather than at the terminating timeout.
#[tokio::test(start_paused = true)]
async fn a_swept_delayed_offer_call_answered_with_its_answer_write_late_acks_rejecting_then_byes() {
    let mut p = place_offerless("swept-delayed-offer-answer-write-late", Callee::Ringing).await;
    depart(&mut p).await;
    return_parked(&mut p).await;
    routable(&mut p);

    let acks = acks_from_b1(&p);
    p.at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    p.ringing.expect(200).await;
    let _answered = p.ringing.ack_with(Some(OFFER)).await;
    p.fh.advance(Duration::from_secs(1)).await;
    let b1 = port(B1);
    assert_eq!(acks_from_b1(&p), acks + 1, "the node ACKs the callee's 2xx at once");
    assert_eq!(
        senders(&p, p.bob.addr(), b"BYE sip:bob@"),
        [b1],
        "the node BYEs the callee at once"
    );

    let bye = p.alice.try_receive_tolerating("BYE", &[]).await;
    bye.expect("the caller is BYEd").respond(200, "OK").await;
    let cancel = p.bob.try_receive_tolerating("CANCEL", &[]).await;
    cancel
        .expect("the backup CANCELs the INVITE its copy holds unanswered")
        .respond(200, "OK")
        .await;
    let ack = p.bob.try_receive_tolerating("ACK", &["CANCEL"]).await;
    let ack = ack.expect("the callee's 2xx is ACKed");
    let answer = String::from_utf8_lossy(ack.request().body()).into_owned();
    assert!(
        answer.lines().any(|l| l.starts_with("m=audio 0 ")),
        "the ACK answers every stream rejected: {answer:?}"
    );
    let bye = p.bob.try_receive_tolerating("BYE", &["CANCEL"]).await;
    bye.expect("the callee is BYEd").respond(200, "OK").await;

    let written =
        p.fh.pump_until(Duration::from_millis(500), Duration::from_secs(60), async || {
            total_cdrs_for(&[&p.w_b1, &p.w_b2], &p.ringing_ref) > 0
        })
        .await;
    assert!(written, "the ended call has its CDR");
    let records = [&p.w_b1, &p.w_b2].map(|w| w.cdr_records());
    let cdrs: Vec<_> = records.iter().flatten().filter(|r| r.call_ref == p.ringing_ref).collect();
    assert_eq!(cdrs.len(), 1, "one CDR for the call");
    let answered =
        cdrs[0].events.iter().any(|e| e.event_type == CdrEventType::Answer && e.reason.is_none());
    assert!(answered, "the CDR records the call answered: {:?}", cdrs[0].events);
    p.proxy.set_health("b1", WorkerHealth::Alive);
    crate::swept_call_primary_returns::hang_up_quiet_and_settle(p, &[]).await;
}

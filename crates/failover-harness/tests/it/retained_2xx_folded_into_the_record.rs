//! **A 2xx the backup's copy retains, folded into the primary's record.** The
//! proxy misroutes the caller's re-INVITE to the backup while the primary
//! lives (ADR-0014); the copy relays it and the callee's 2xx, which is lost on
//! its way to the caller. The copy's writes reach the primary, whose record
//! then holds the answered round and its retained 2xx. Read alive again, the
//! primary receives the caller's retransmission of the re-INVITE: its
//! transaction layer holds that INVITE's server transaction in RFC 6026
//! Accepted, seeded by the fold, and answers the copy with the retained 2xx —
//! no 100 and no 491. The caller's ACK, through the primary, reaches the
//! callee once, and the call ends with one CDR.
//!
//! ```text
//!   alice ─INVITE─▶ COPY ─INVITE─▶ bob ─200─▶ COPY ─200─╳ alice
//!                   COPY ══ write ══▶ PRIMARY (folds the round and its 2xx)
//!   alice ─INVITE─▶ PRIMARY ─200 (the retained one)─▶ alice
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{FailoverHarness, ReplicatedB2buaSut, WorkerHealth};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::crossed_reinvite_round::caller_hangs_up;
use crate::inflight_resend::last_request_sent;
use crate::reinvite_2xx_acked_through_the_primary::re_ack_copies;

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

/// The statuses of the INVITE responses `from` put on the wire toward `to`
/// after the first `since` entries of the recorded wire.
fn invite_responses(
    fh: &FailoverHarness,
    since: usize,
    from: SocketAddr,
    to: SocketAddr,
) -> Vec<u16> {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .skip(since)
        .filter(|e| e.from == from && e.to == to)
        .filter_map(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Response(r)) if *r.cseq().method() == Method::Invite => Some(r.status()),
            _ => None,
        })
        .collect()
}

/// The primary and the backup.
fn nodes(est: &Established) -> (&ReplicatedB2buaSut, &ReplicatedB2buaSut) {
    if est.primary_ord == "b1" {
        (&est.w_b1, &est.w_b2)
    } else {
        (&est.w_b2, &est.w_b1)
    }
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_retransmitted_to_the_primary_draws_the_2xx_its_copy_retained() {
    let mut est = establish("retained-2xx-folded-into-the-record").await;
    let p_ord = est.primary_ord.clone();

    // ── The round goes through the copy; its 2xx is lost to the caller ──────
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let round = est.dialog.send_request(InDialogMethod::Invite).with_sdp(OFFER).send().await;
    let cseq = round.sent_invite().expect("the re-INVITE").cseq().seq();
    est.fh.cut_signalling("alice", est.alice.addr());
    let mut at_bob = est.bob.receive("INVITE").await;
    est.fh.advance(SETTLE).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    est.fh.advance(SETTLE).await;
    let (primary, copy) = nodes(&est);
    assert!(copy.serves(&est.call_ref), "the copy serves the round");
    let record = primary.live_call(&est.call_ref).expect("the primary serves the call");
    assert!(
        record.a_leg.dialogs.iter().any(|d| d.pending_reinvite_2xx.is_some()),
        "the primary's record holds the retained 2xx the copy's write carried",
    );

    // ── Read alive, the primary receives the caller's Timer A rung ──────────
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    est.fh.advance(SETTLE).await;
    est.fh.restore_signalling("alice", est.alice.addr());
    let since = est.fh.sip_entries().len();
    let (wire, dst) = last_request_sent(&est.fh, est.alice.addr(), &Method::Invite);
    est.alice.try_send_datagram(&wire, dst).await.expect("the caller's re-send leaves");
    est.fh.advance(Duration::from_millis(200)).await;
    let answered = invite_responses(&est.fh, since, nodes(&est).0.sip_addr(), est.proxy.addr());
    assert!(
        !answered.is_empty() && answered.iter().all(|s| *s == 200),
        "the primary answers the copy with the retained 2xx alone — no 100, no 491: {answered:?}",
    );

    // ── The caller ACKs through the primary; the callee draws one ACK ───────
    est.dialog.ack_for(cseq, None).await;
    let acks = re_ack_copies(&est.fh, &est.alice, &mut est.dialog, &est.bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");
    caller_hangs_up(&mut est).await;
}

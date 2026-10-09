//! **RFC 3262 §4 — the callee's final finds every relayed reliable provisional
//! of that INVITE acknowledged.** The caller the provisionals were shown to may
//! never PRACK them; the final then ends her chance to, so this stack, the
//! callee leg's UAC and the only party that took them, PRACKs each one still
//! unacknowledged on its own early dialog as the final arrives — a 2xx or a
//! rejection alike — and then handles the final as usual.
//!
//! **RFC 3262 §3 — a late PRACK naming a provisional this stack showed is
//! answered 2xx here.** A caller's PRACK reaching this stack after the INVITE's
//! final names a provisional it really sent on her face; the PRACK's own
//! transaction does not end with the INVITE's, and the responder is already
//! acknowledged, so this face answers it and relays nothing (one PRACK per
//! `RSeq`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallFailureResponse, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, stated, stated_by_response, B2buaSut};
use scenario_harness::{Harness, ServerTxn, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_net::RecordedSipEntry;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The callee's own `RSeq`, far from anything this stack mints.
const BOB_RSEQ: u32 = 4711;

/// The caller withholds her PRACK: that is what leaves the callee's
/// provisional for this stack to acknowledge.
fn caller_withholds_prack(h: &Harness) {
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice never PRACKs the provisional shown to her before the final — the PRACK this \
             stack owes the callee in her place is the subject",
        )
        .on_party("alice"),
    );
}

fn reliable_180(uas: &mut ServerTxn) -> scenario_harness::Respond<'_> {
    uas.respond(180, "Ringing")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
}

/// The PRACKs the SUT put on `to`'s wire, as their `RAck` values in send order.
fn pracks_to(entries: &[RecordedSipEntry], from: SocketAddr, to: SocketAddr) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(b"PRACK "))
        .filter(|e| e.reemit.is_none())
        .filter_map(|e| {
            let text = std::str::from_utf8(&e.raw).ok()?;
            text.split("\r\n")
                .find(|l| l.len() > 5 && l[..5].eq_ignore_ascii_case("RAck:"))
                .map(|l| l[5..].trim().to_string())
        })
        .collect()
}

/// The recording index of the first SUT message to `to` starting with `head`.
fn first_index(
    entries: &[RecordedSipEntry],
    from: SocketAddr,
    to: SocketAddr,
    head: &[u8],
) -> Option<usize> {
    entries.iter().position(|e| e.from == from && e.to == to && e.raw.starts_with(head))
}

/// The callee answers 200 over a reliable ring the caller never PRACKed: the
/// stack PRACKs the ring on its early dialog before it relays the 2xx, once.
#[tokio::test(start_paused = true)]
async fn a_2xx_over_an_unpracked_ring_is_preceded_by_the_stacks_prack() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-2xx", 0);
    caller_withholds_prack(&h);
    let alice = h.agent("alice", "127.0.0.1:7401").await;
    let bob = h.agent("bob", "127.0.0.1:7402").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7402).start(&h, "b2bua", "127.0.0.1:7403").await;
    let alice_addr: SocketAddr = "127.0.0.1:7401".parse().unwrap();
    let bob_addr: SocketAddr = "127.0.0.1:7402".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let invite_cseq = uas.request().cseq().seq();
    reliable_180(&mut uas).await;
    call.expect(180).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let mut prack = bob.receive("PRACK").await;
    assert_eq!(
        stated(prack.request(), "RAck").as_deref(),
        Some(format!("{BOB_RSEQ} {invite_cseq} INVITE").as_str()),
        "the PRACK names the callee's own provisional",
    );
    prack.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let entries = report.entries();
    assert_eq!(pracks_to(&entries, b2bua.addr, bob_addr).len(), 1, "one PRACK per RSeq");
    let pracked = first_index(&entries, b2bua.addr, bob_addr, b"PRACK ").unwrap();
    let answered = first_index(&entries, b2bua.addr, alice_addr, b"SIP/2.0 200 ").unwrap();
    assert!(pracked < answered, "the PRACK leaves before the 2xx is relayed");
}

/// The caller's PRACK reaches this stack only after it relayed the 2xx it
/// PRACKed the callee for: answered here, never relayed as a second PRACK.
#[tokio::test(start_paused = true)]
async fn a_caller_prack_after_the_stacks_own_is_answered_here() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-2xx-late", 0);
    let alice = h.agent("alice", "127.0.0.1:7404").await;
    let bob = h.agent("bob", "127.0.0.1:7405").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7405).start(&h, "b2bua", "127.0.0.1:7406").await;
    let bob_addr: SocketAddr = "127.0.0.1:7405".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    let ringing = call.expect(180).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    call.expect(200).await;
    // Her PRACK crossed the 2xx on the wire.
    let mut late = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    late.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(
        pracks_to(&report.entries(), b2bua.addr, bob_addr).len(),
        1,
        "one PRACK per RSeq toward the callee"
    );
}

/// The callee rejects the INVITE over a reliable ring the caller never
/// PRACKed: the stack PRACKs the ring as the rejection arrives and relays the
/// rejection as usual.
#[tokio::test(start_paused = true)]
async fn a_rejection_over_an_unpracked_ring_is_pracked_by_the_stack() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-reject", 0);
    caller_withholds_prack(&h);
    let alice = h.agent("alice", "127.0.0.1:7407").await;
    let bob = h.agent("bob", "127.0.0.1:7408").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7408).start(&h, "b2bua", "127.0.0.1:7409").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let invite_cseq = uas.request().cseq().seq();
    reliable_180(&mut uas).await;
    call.expect(180).await;

    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    let mut prack = bob.receive("PRACK").await;
    assert_eq!(
        stated(prack.request(), "RAck").as_deref(),
        Some(format!("{BOB_RSEQ} {invite_cseq} INVITE").as_str()),
    );
    prack.respond(200, "OK").await;
    call.expect(486).await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The caller's PRACK reaches this stack after it relayed the callee's
/// rejection: it names a provisional this stack showed her, so it draws 200
/// here, and the callee — PRACKed once already — receives nothing more.
#[tokio::test(start_paused = true)]
async fn a_late_caller_prack_after_the_rejection_is_answered_here() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-reject-late", 0);
    let alice = h.agent("alice", "127.0.0.1:7410").await;
    let bob = h.agent("bob", "127.0.0.1:7411").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7411).start(&h, "b2bua", "127.0.0.1:7412").await;
    let bob_addr: SocketAddr = "127.0.0.1:7411".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    let ringing = call.expect(180).await;

    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    call.expect(486).await;
    h.advance(Duration::from_millis(70)).await;
    let mut late = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    late.expect(200).await;
    assert_eq!(b2bua.metrics().late_prack_answered_total(), 1, "counted as a late PRACK");
    assert_eq!(
        b2bua.metrics().unroutable_refused_total(),
        0,
        "a late PRACK answered 200 is no unroutable refusal"
    );

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(
        pracks_to(&report.entries(), b2bua.addr, bob_addr).len(),
        1,
        "one PRACK per RSeq toward the callee"
    );
}

/// A forked callee rings reliably on two early dialogs, the caller PRACKs
/// neither, and the second fork answers: each fork's provisional is PRACKed on
/// its own dialog — its To-tag, its own `RSeq`, that dialog's own CSeq.
#[tokio::test(start_paused = true)]
async fn each_forks_unpracked_ring_is_pracked_on_its_own_dialog_at_the_2xx() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-forked", 0);
    caller_withholds_prack(&h);
    let alice = h.agent("alice", "127.0.0.1:7413").await;
    let bob = h.agent("bob", "127.0.0.1:7414").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7414).start(&h, "b2bua", "127.0.0.1:7415").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut b_inv = bob.receive("INVITE").await;
    let invite_cseq = b_inv.request().cseq().seq();
    for (tag, rseq) in [("fork1", "11"), ("fork2", "21")] {
        b_inv
            .respond(180, "Ringing")
            .with_to_tag(tag)
            .with_header("Require", "100rel")
            .with_header("RSeq", rseq)
            .await;
        call.expect(180).await;
    }

    b_inv.respond(200, "OK").with_to_tag("fork2").with_sdp(ANSWER).await;
    let mut seen = Vec::new();
    for _ in 0..2 {
        let mut prack = bob.receive("PRACK").await;
        let req = prack.request();
        seen.push((
            req.to().tag().unwrap_or_default().to_string(),
            stated(req, "RAck").unwrap_or_default(),
            req.cseq().seq(),
        ));
        prack.respond(200, "OK").await;
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("fork1".to_string(), format!("11 {invite_cseq} INVITE"), invite_cseq + 1),
            ("fork2".to_string(), format!("21 {invite_cseq} INVITE"), invite_cseq + 1),
        ],
        "one PRACK per fork, on its own dialog and sequence"
    );
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The first callee rings reliably, the caller does not PRACK, and the callee
/// fails: the stack PRACKs the ring as the failure arrives, then reroutes; the
/// second callee answers.
#[tokio::test(start_paused = true)]
async fn a_failure_that_reroutes_is_pracked_before_the_reroute() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-reroute", 0);
    caller_withholds_prack(&h);
    let alice = h.agent("alice", "127.0.0.1:7416").await;
    let bob1 = h.agent("bob1", "127.0.0.1:7417").await;
    let bob2 = h.agent("bob2", "127.0.0.1:7418").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 7417);
                r.callback_context = Some("reroute-on-failure".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_req| CallFailureResponse::Route(route_to("127.0.0.1", 7418)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:7419").await;

    let mut call = alice
        .invite(&bob1)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas1 = bob1.receive("INVITE").await;
    let invite_cseq = uas1.request().cseq().seq();
    reliable_180(&mut uas1).await;
    call.expect(180).await;

    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await;
    let mut prack = bob1.receive("PRACK").await;
    assert_eq!(
        stated(prack.request(), "RAck").as_deref(),
        Some(format!("{BOB_RSEQ} {invite_cseq} INVITE").as_str()),
    );
    prack.respond(200, "OK").await;

    let mut uas2 = bob2.receive("INVITE").await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob2.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob2.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The callee answers the stack's own PRACK 481 after his 2xx: that refusal
/// denies the one PRACK transaction, never the call (RFC 3261 §14.1 by
/// analogy), which stays up until the caller's BYE.
#[tokio::test(start_paused = true)]
async fn a_refused_own_prack_at_the_2xx_keeps_the_call() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-refused", 0);
    caller_withholds_prack(&h);
    h.waive(
        WaiverScope::rule(
            "prack-2xx-or-481",
            "bob deliberately refuses the PRACK of a provisional he sent — the call surviving it \
             is the subject",
        )
        .on_party("bob"),
    );
    let alice = h.agent("alice", "127.0.0.1:7420").await;
    let bob = h.agent("bob", "127.0.0.1:7421").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7421).start(&h, "b2bua", "127.0.0.1:7422").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    call.expect(180).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob.receive("PRACK").await.respond(481, "Call/Transaction Does Not Exist").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(2)).await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The callee never answers the stack's own PRACK after his 2xx: its timeout
/// denies that transaction only, and the call stays up until the caller's BYE.
#[tokio::test(start_paused = true)]
async fn an_unanswered_own_prack_at_the_2xx_keeps_the_call() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-unanswered", 0);
    caller_withholds_prack(&h);
    let alice = h.agent("alice", "127.0.0.1:7423").await;
    let bob = h.agent("bob", "127.0.0.1:7424").await;
    // No keepalive inside the PRACK's 64·T1: the subject is that one transaction.
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 7424)
        .tune(|c| c.keepalive_interval_sec = 120)
        .start(&h, "b2bua", "127.0.0.1:7425")
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    call.expect(180).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let _unanswered = bob.receive("PRACK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    // Past the PRACK client transaction's Timer F (64·T1).
    h.advance(Duration::from_secs(40)).await;
    bob.drain().await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The caller CANCELs, then PRACKs the ring she was shown while this stack
/// still waits for the callee's 487: the stack PRACKed the callee as it
/// CANCELled, so her PRACK draws 200 here and nothing reaches the callee.
#[tokio::test(start_paused = true)]
async fn a_caller_prack_after_her_cancel_is_answered_while_the_call_tears_down() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-cancel-late", 0);
    let alice = h.agent("alice", "127.0.0.1:7426").await;
    let bob = h.agent("bob", "127.0.0.1:7427").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7427).start(&h, "b2bua", "127.0.0.1:7428").await;
    let bob_addr: SocketAddr = "127.0.0.1:7427".parse().unwrap();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    reliable_180(&mut uas).await;
    let ringing = call.expect(180).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    // The callee's 487 is still to come: the call is tearing down.
    let mut late = call.try_prack(&ringing).await.expect("alice PRACKs the reliable 180");
    late.expect(200).await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(
        pracks_to(&report.entries(), b2bua.addr, bob_addr).len(),
        1,
        "one PRACK per RSeq toward the callee"
    );
}

/// The callee's own offer, carried by its reliable provisional on a
/// delayed-offer INVITE.
const BOB_OFFER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The caller's answer to [`BOB_OFFER`].
const ALICE_ANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

fn reliable_183_offer(uas: &mut ServerTxn) -> scenario_harness::Respond<'_> {
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(BOB_OFFER)
}

/// A delayed-offer INVITE: the callee's reliable 183 carries the offer, the
/// caller never PRACKs it, and the callee rejects the INVITE. The PRACK the
/// stack owes answers that offer (RFC 3262 §5), rejecting every stream
/// (RFC 3264 §6) — the call it belonged to failed.
#[tokio::test(start_paused = true)]
async fn a_rejected_delayed_offer_is_answered_rejecting_in_the_stacks_prack() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-delayed-reject", 0);
    caller_withholds_prack(&h);
    let alice = h.agent("alice", "127.0.0.1:7429").await;
    let bob = h.agent("bob", "127.0.0.1:7430").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7430).start(&h, "b2bua", "127.0.0.1:7431").await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert!(uas.request().sdp().is_none(), "a delayed offer: the INVITE carries none");
    reliable_183_offer(&mut uas).await;
    call.expect(183).await;

    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    let mut prack = bob.receive("PRACK").await;
    let body = String::from_utf8_lossy(prack.request().body()).into_owned();
    assert!(
        body.lines().any(|l| l.starts_with("m=audio 0 ")),
        "the PRACK answers the offer, rejecting its stream: {body:?}"
    );
    prack.respond(200, "OK").await;
    call.expect(486).await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// A delayed-offer INVITE whose callee answers 200 over its reliable 183
/// offer, never PRACKed by the caller. The caller answers that offer on her
/// ACK; the stack's PRACK carries that same answer toward the callee
/// (RFC 3262 §5), so both faces agree on one session (RFC 3264), and the ACK
/// relayed after it carries none — the exchange closed in the PRACK.
#[tokio::test(start_paused = true)]
async fn a_2xx_over_an_unpracked_delayed_offer_is_answered_in_the_prack_with_the_callers_answer() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-delayed-2xx", 0);
    caller_withholds_prack(&h);
    h.waive(
        WaiverScope::rule(
            "delay-2xx-on-unacked-reliable-1xx-with-sdp",
            "bob deliberately answers 200 before his reliable offer is PRACKed",
        )
        .on_party("bob"),
    );
    h.waive(
        WaiverScope::rule(
            "delay-2xx-on-unacked-reliable-1xx-with-sdp",
            "the SUT relays bob's early 200 toward alice, who never PRACKs the 183 it showed her",
        )
        .on_party("b2bua"),
    );
    let alice = h.agent("alice", "127.0.0.1:7432").await;
    let bob = h.agent("bob", "127.0.0.1:7433").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7433).start(&h, "b2bua", "127.0.0.1:7434").await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    reliable_183_offer(&mut uas).await;
    call.expect(183).await;

    uas.respond(200, "OK").with_sdp(BOB_OFFER).await;
    call.expect(200).await;
    let mut dialog = call.ack_with(Some(ALICE_ANSWER)).await;
    let mut prack = bob.receive("PRACK").await;
    assert_eq!(
        String::from_utf8_lossy(prack.request().body()),
        ALICE_ANSWER,
        "the PRACK carries the caller's own answer to the offer"
    );
    prack.respond(200, "OK").await;
    let ack = bob.receive("ACK").await;
    assert!(ack.request().body().is_empty(), "the exchange closed in the PRACK");
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The second fork's own offer, carried by its 2xx.
const FORK2_OFFER: &str = "v=0\r\no=bob 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

/// The answer the caller's later ACK gives to a re-offer.
const ALICE_REANSWER: &str = "v=0\r\no=alice 2 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

/// A delayed-offer INVITE forks: fork1's reliable 183 carries an offer the
/// caller never PRACKs, fork2 answers 200 with its own. Fork1's provisional is
/// PRACKed as the 2xx arrives, its offer answered rejecting every stream; the
/// caller's answer on her ACK answers fork2's offer and reaches fork2 on the
/// relayed ACK (RFC 3261 §13.2.2.4) — and so does the answer on the ACK of a
/// later offerless re-INVITE on that dialog.
#[tokio::test(start_paused = true)]
async fn a_losing_forks_delayed_offer_is_pracked_at_the_2xx_and_the_winners_ack_keeps_its_answer() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-forked-delayed", 0);
    caller_withholds_prack(&h);
    let alice = h.agent("alice", "127.0.0.1:7435").await;
    let bob = h.agent("bob", "127.0.0.1:7436").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7436).start(&h, "b2bua", "127.0.0.1:7437").await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv
        .respond(183, "Session Progress")
        .with_to_tag("fork1")
        .with_header("Require", "100rel")
        .with_header("RSeq", "11")
        .with_sdp(BOB_OFFER)
        .await;
    call.expect(183).await;

    b_inv.respond(200, "OK").with_to_tag("fork2").with_sdp(FORK2_OFFER).await;
    let mut prack = bob.receive("PRACK").await;
    assert_eq!(prack.request().to().tag(), Some("fork1"), "the losing fork's own dialog");
    let body = String::from_utf8_lossy(prack.request().body()).into_owned();
    assert!(
        body.lines().any(|l| l.starts_with("m=audio 0 ")),
        "the losing fork's offer is answered rejecting its stream: {body:?}"
    );
    prack.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack_with(Some(ALICE_ANSWER)).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(
        String::from_utf8_lossy(ack.request().body()),
        ALICE_ANSWER,
        "the winner's ACK carries the caller's answer"
    );

    let mut reinv = dialog.send_request(InDialogMethod::Invite).send().await;
    let mut re_uas = bob.receive("INVITE").await;
    re_uas.respond(200, "OK").with_sdp(BOB_OFFER).await;
    let ok = reinv.expect(200).await;
    dialog.ack_for(ok.cseq().seq(), Some(ALICE_REANSWER)).await;
    let re_ack = bob.receive("ACK").await;
    assert_eq!(
        String::from_utf8_lossy(re_ack.request().body()),
        ALICE_REANSWER,
        "a later delayed-offer ACK on the leg keeps its answer"
    );
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// A delayed-offer INVITE answered 200 over its reliable 183 offer; the
/// caller does not PRACK in time and never ACKs, so the 2xx's give-up ends the
/// call before any ACK could carry her answer. The PRACK still owed leaves ahead of the
/// callee's BYE, answering the offer rejecting every stream (RFC 3262 §4-§5);
/// her PRACK crossing the teardown draws 200 here, one naming nothing 481.
#[tokio::test(start_paused = true)]
async fn a_deferred_offer_is_pracked_before_a_teardown_that_comes_before_the_ack() {
    const ACK_TIMEOUT_SEC: u64 = 6;
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-deferred-teardown", 0);
    h.waive(
        WaiverScope::rule(
            "no-ack-to-dialog-creating-2xx",
            "alice deliberately never ACKs — the teardown before her answer is the subject",
        )
        .on_party("alice"),
    );
    h.waive(
        WaiverScope::rule(
            "delay-2xx-on-unacked-reliable-1xx-with-sdp",
            "bob deliberately answers 200 before his reliable offer is PRACKed",
        )
        .on_party("bob"),
    );
    h.waive(
        WaiverScope::rule(
            "delay-2xx-on-unacked-reliable-1xx-with-sdp",
            "the SUT relays bob's early 200 toward alice, who never PRACKs the 183 it showed her",
        )
        .on_party("b2bua"),
    );
    let alice = h.agent("alice", "127.0.0.1:7438").await;
    let bob = h.agent("bob", "127.0.0.1:7439").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 7439)
        .tune(|c| c.ack_timeout_sec = ACK_TIMEOUT_SEC as i64)
        .start(&h, "b2bua", "127.0.0.1:7440")
        .await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    reliable_183_offer(&mut uas).await;
    let progress = call.expect(183).await;
    uas.respond(200, "OK").with_sdp(BOB_OFFER).await;
    call.expect(200).await;

    h.advance(Duration::from_secs(ACK_TIMEOUT_SEC) + Duration::from_millis(100)).await;
    alice.drain().await;
    // Bob sees PRACK, ACK, BYE: the PRACK closes the offer/answer exchange
    // the 2xx left open, so the ACK that follows owes no body (RFC 3262 §5,
    // RFC 3261 §13.2.2.4) — the order of the caller's own ACK path.
    let mut prack = bob.receive("PRACK").await;
    let body = String::from_utf8_lossy(prack.request().body()).into_owned();
    assert!(
        body.lines().any(|l| l.starts_with("m=audio 0 ")),
        "the owed PRACK answers the offer, rejecting its stream: {body:?}"
    );
    prack.respond(200, "OK").await;
    let ack = bob.receive("ACK").await;
    assert!(ack.request().body().is_empty(), "the answer rode the PRACK");
    assert_eq!(ack.request().cseq().seq(), uas.request().cseq().seq(), "the 2xx's INVITE");
    let mut bob_bye = bob.receive("BYE").await;

    // Her PRACK crosses the teardown: it names the 183 she was shown, and
    // answers its offer.
    let shown_rseq = stated_by_response(&progress, "RSeq").expect("a reliable 183");
    let mut late = call
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{shown_rseq} {} INVITE", progress.cseq().seq()))
        .with_sdp(ALICE_ANSWER)
        .try_send()
        .await
        .expect("alice PRACKs the reliable 183");
    late.expect(200).await;
    let mut stray = call
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{} {} INVITE", BOB_RSEQ + 1000, progress.cseq().seq()))
        .try_send()
        .await
        .expect("alice sends a PRACK naming nothing");
    stray.expect(481).await;

    alice.receive("BYE").await.respond(200, "OK").await;
    bob_bye.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// A delayed-offer INVITE whose offer rode a reliable 183 the caller PRACKed
/// with her answer: the exchange closed in that PRACK (RFC 3262 §5), so the
/// 2xx's description is no new offer and the caller's bare ACK reaches the
/// callee bare (RFC 3264 §4).
#[tokio::test(start_paused = true)]
async fn an_offer_answered_in_the_callers_prack_leaves_the_ack_bare() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-offer-pracked", 0);
    let alice = h.agent("alice", "127.0.0.1:7441").await;
    let bob = h.agent("bob", "127.0.0.1:7442").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7442).start(&h, "b2bua", "127.0.0.1:7443").await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    reliable_183_offer(&mut uas).await;
    let progress = call.expect(183).await;
    let shown_rseq = stated_by_response(&progress, "RSeq").expect("a reliable 183");
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{shown_rseq} {} INVITE", progress.cseq().seq()))
        .with_sdp(ALICE_ANSWER)
        .try_send()
        .await
        .expect("alice PRACKs the 183 with her answer");
    let mut bob_prack = bob.receive("PRACK").await;
    assert!(!bob_prack.request().body().is_empty(), "her answer is relayed");
    bob_prack.respond(200, "OK").await;
    prack.expect(200).await;

    uas.respond(200, "OK").with_sdp(BOB_OFFER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    let ack = bob.receive("ACK").await;
    assert!(
        ack.request().body().is_empty(),
        "the exchange closed in the PRACK: {:?}",
        String::from_utf8_lossy(ack.request().body())
    );
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// A losing fork's reliable 183 carried the offer and was PRACKed — by the
/// stack as the winning fork's 2xx arrived, or by the caller with her answer;
/// that fork's own late 2xx is ACKed bare and BYEd: its offer/answer exchange
/// closed in the PRACK (RFC 3262 §5, RFC 3264 §4).
async fn losing_fork_late_2xx_is_acked_bare(caller_pracks: bool, ports: [&str; 3]) {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-final-straggler", 0);
    if !caller_pracks {
        caller_withholds_prack(&h);
    }
    let alice = h.agent("alice", ports[0]).await;
    let bob = h.agent("bob", ports[1]).await;
    let bob_port: u16 = ports[1].rsplit(':').next().unwrap().parse().unwrap();
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob_port).start(&h, "b2bua", ports[2]).await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv
        .respond(183, "Session Progress")
        .with_to_tag("fork1")
        .with_header("Require", "100rel")
        .with_header("RSeq", "11")
        .with_sdp(BOB_OFFER)
        .await;
    let progress = call.expect(183).await;
    if caller_pracks {
        let shown_rseq = stated_by_response(&progress, "RSeq").expect("a reliable 183");
        // Fork-addressed: the PRACK rides the early dialog the 183 created,
        // on that dialog's own CSeq sequence.
        let mut prack = call
            .send_request(InDialogMethod::Prack)
            .with_to_tag(progress.to().tag().expect("a tagged 183"))
            .with_rack(&format!("{shown_rseq} {} INVITE", progress.cseq().seq()))
            .with_sdp(ALICE_ANSWER)
            .try_send()
            .await
            .expect("alice PRACKs fork1's 183 with her answer");
        bob.receive("PRACK").await.respond(200, "OK").await;
        prack.expect(200).await;
    }

    b_inv.respond(200, "OK").with_to_tag("fork2").with_sdp(FORK2_OFFER).await;
    if !caller_pracks {
        bob.receive("PRACK").await.respond(200, "OK").await;
    }
    call.expect(200).await;
    let mut dialog = call.ack_with(Some(ALICE_ANSWER)).await;
    bob.receive("ACK").await;

    // Fork1 answers too, late.
    b_inv.respond(200, "OK").with_to_tag("fork1").with_sdp(BOB_OFFER).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some("fork1"));
    assert!(
        ack.request().body().is_empty(),
        "fork1's exchange closed in its PRACK: {:?}",
        String::from_utf8_lossy(ack.request().body())
    );
    let mut fork1_bye = bob.receive("BYE").await;
    assert_eq!(fork1_bye.request().to().tag(), Some("fork1"));
    fork1_bye.respond(200, "OK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_losing_forks_late_2xx_after_the_stacks_prack_is_acked_bare() {
    losing_fork_late_2xx_is_acked_bare(
        false,
        ["127.0.0.1:7444", "127.0.0.1:7445", "127.0.0.1:7446"],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_losing_forks_late_2xx_after_the_callers_prack_is_acked_bare() {
    losing_fork_late_2xx_is_acked_bare(
        true,
        ["127.0.0.1:7447", "127.0.0.1:7448", "127.0.0.1:7449"],
    )
    .await;
}

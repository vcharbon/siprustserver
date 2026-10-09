//! **RFC 3262 §4 — a CANCEL this stack sends PRACKs every relayed reliable
//! provisional still unacknowledged on that INVITE.** The caller the
//! provisionals were shown to will not PRACK them any more, and the CANCEL does
//! not end the INVITE transaction, so this stack, the callee leg's UAC, owes
//! each acknowledgement — on the provisional's own early dialog.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallFailureResponse, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, stated, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The caller withholds her PRACKs: that is what leaves the callee's
/// provisionals for this stack to acknowledge.
fn caller_withholds_prack(h: &Harness) {
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice never PRACKs the provisionals shown to her — the PRACKs this stack owes the \
             callee in her place are the subject",
        )
        .on_party("alice"),
    );
}

/// A forked callee rings reliably on two early dialogs; the caller CANCELs.
/// Each fork's provisional is PRACKed on its own dialog: its To-tag, its own
/// `RSeq`, and that dialog's own CSeq sequence (RFC 3261 §12.2.1.1).
#[tokio::test(start_paused = true)]
async fn each_forks_reliable_provisional_is_pracked_on_its_own_dialog() {
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-cancel-forked", 1);
    caller_withholds_prack(&h);
    let alice = h.agent("alice", "127.0.0.1:5221").await;
    let bob = h.agent("bob", "127.0.0.1:5222").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5222).start(&h, "b2bua", "127.0.0.1:5223").await;

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

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
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
    bob.receive("CANCEL").await.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").with_to_tag("fork1").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The first callee rings reliably until the route's no-answer deadline: this
/// stack destroys that leg and reroutes while the caller stays. The CANCEL that
/// ends the abandoned leg comes with the PRACK its provisional is owed.
#[tokio::test(start_paused = true)]
async fn a_leg_abandoned_at_its_no_answer_deadline_is_pracked_as_it_is_cancelled() {
    const NO_ANSWER_SEC: i64 = 5;
    let h = Harness::with_transit_delay("b2bua-prack-owed-at-cancel-no-answer", 0);
    caller_withholds_prack(&h);
    let alice = h.agent("alice", "127.0.0.1:5224").await;
    let bob1 = h.agent("bob1", "127.0.0.1:5225").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5226").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5225);
                r.callback_context = Some("no-answer-reroute".into());
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                NewCallResponse::Route(r)
            })
            .on_failure(|_req| CallFailureResponse::Route(route_to("127.0.0.1", 5226)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5227").await;

    let mut call = alice
        .invite(&bob1)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").with_header("Require", "100rel").with_header("RSeq", "31").await;
    call.expect(180).await;

    // ── the no-answer deadline destroys the ringing leg ──
    h.advance(Duration::from_secs(NO_ANSWER_SEC as u64) + Duration::from_millis(100)).await;
    alice.drain().await;
    let mut prack = bob1.receive("PRACK").await;
    assert_eq!(
        stated(prack.request(), "RAck").as_deref(),
        Some(format!("31 {} INVITE", uas1.request().cseq().seq()).as_str()),
    );
    prack.respond(200, "OK").await;
    bob1.receive("CANCEL").await.respond(200, "OK").await;
    uas1.respond(487, "Request Terminated").await;
    bob1.receive("ACK").await;

    // ── the call lives on: the reroute answers it ──
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

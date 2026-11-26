//! RFC 3261 §13.2.2.4 — a dialog this stack BYEs while its 2xx carrying an
//! offer is still unACKed is ACKed first, and the ACK carries an answer.
//!
//! The caller INVITEs without a description, so the callee's 2xx holds the
//! offer and its answer rides the ACK (RFC 3264 §4); on the b-leg that ACK is
//! the caller's, relayed. While she holds it, the call's duration cap ends the
//! call: "the UAC core MUST generate a valid answer in the ACK and then send a
//! BYE immediately". The stack has no answer of its own, so its ACK answers
//! with every stream rejected (RFC 3264 §6), then the BYE leaves. The caller's
//! late ACK, which would have carried the answer, is absorbed: the callee's
//! 2xx is ACKed once. A copy of that 2xx draws the same ACK again, answer
//! included. The caller's BYE waits for her ACK (RFC 3261 §15). The same
//! holds for a relayed offerless re-INVITE whose 2xx makes the offer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use call::features::MaxDurationAnchor;
use call::TerminationCause;
use scenario_harness::{Agent, Harness};
use sip_message::generators::InDialogMethod;
use sip_message::SipMessage;

const OFFER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\nm=video 20002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
const ANSWER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\nm=video 10002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";

/// The cap, from the answer: it fires inside the first rung (T1, 500 ms) of
/// the 2xx ladder toward the caller.
const CAP: Duration = Duration::from_millis(300);

/// The callee's offer-carrying 2xx awaits the caller's ACK when the cap ends
/// the call: the callee gets an ACK rejecting every stream, then the BYE; a
/// copy of his 2xx gets that ACK again; the caller's late ACK reaches no one,
/// and her BYE follows it.
#[tokio::test(start_paused = true)]
async fn a_dialog_byed_while_its_ack_owes_an_answer_is_acked_rejecting_then_byed() {
    let h = Harness::with_transit_delay("b2bua-release-while-answer-owed-in-ack", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = capped_b2bua(&h, &bob).await;

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    let invite_cseq = uas.request().cseq().seq();
    assert!(uas.request().body().is_empty(), "the INVITE leaves the offer to bob");
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    call.expect(200).await;

    h.advance(CAP + Duration::from_millis(10)).await;
    let ack = bob
        .try_receive("ACK")
        .await
        .expect("RFC 3261 §13.2.2.4: the 2xx is ACKed before its dialog is BYEd");
    let answer = String::from_utf8_lossy(ack.request().body()).into_owned();
    assert_eq!(ack.request().cseq().seq(), invite_cseq, "the ACK takes the INVITE's CSeq");
    assert_every_stream_rejected(&answer);

    uas.respond(200, "OK").with_sdp(OFFER).await;
    h.advance(Duration::from_millis(10)).await;
    let mut bob_bye = bob.receive("BYE").await;
    while let Some(m) = alice.take_queued().await {
        assert!(!matches!(m, SipMessage::Request(_)), "the caller's BYE waits for her ACK: {m:?}");
    }

    let _dialog = call.ack_with(Some(ANSWER)).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.drain().await;
    bob_bye.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one call record: {cdrs:#?}");
    let termination = cdrs[0].termination.as_ref().expect("the record states the call's end");
    assert_eq!(termination.cause, TerminationCause::MaxDuration);

    let report = h.finish().await;

    let acks = acks_to(&report, b2bua.addr, bob.addr(), invite_cseq);
    assert_eq!(
        acks.len(),
        2,
        "one ACK and its repeat for the copy; the caller's late ACK is absorbed: got {}",
        acks.len(),
    );
    assert_eq!(
        String::from_utf8_lossy(&acks[1]),
        String::from_utf8_lossy(&acks[0]),
        "RFC 3261 §13.2.2.4: the 2xx's copy draws the same ACK, answer included",
    );
}

const INITIAL_OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const INITIAL_ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20004 RTP/AVP 0\r\nm=video 20006 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
const REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\nm=video 10006 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";

/// The caller's offerless re-INVITE is relayed, and the callee's 2xx to it
/// makes the offer her ACK answers (RFC 3264 §4). While she holds that ACK
/// the cap ends the call: the callee gets an ACK on the re-INVITE's CSeq
/// rejecting every stream, then the BYE; the caller's late ACK reaches no one,
/// and her BYE follows it.
#[tokio::test(start_paused = true)]
async fn a_dialog_byed_while_its_reinvite_ack_owes_an_answer_is_acked_rejecting_then_byed() {
    let h = Harness::with_transit_delay("b2bua-release-while-reinvite-answer-owed-in-ack", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = capped_b2bua(&h, &bob).await;

    let mut call = alice.invite(&bob).with_sdp(INITIAL_OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(INITIAL_ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinvite = dialog.request(InDialogMethod::Invite, None).await;
    let mut bob_reinvite = bob.receive("INVITE").await;
    let reinvite_cseq = bob_reinvite.request().cseq().seq();
    assert!(bob_reinvite.request().body().is_empty(), "the re-INVITE leaves the offer to bob");
    bob_reinvite.respond(200, "OK").with_sdp(REOFFER).await;
    reinvite.expect(200).await;

    h.advance(CAP + Duration::from_millis(10)).await;
    let ack = bob
        .try_receive("ACK")
        .await
        .expect("RFC 3261 §13.2.2.4: the re-INVITE's 2xx is ACKed before its dialog is BYEd");
    assert_eq!(ack.request().cseq().seq(), reinvite_cseq, "the ACK takes the re-INVITE's CSeq");
    assert_every_stream_rejected(&String::from_utf8_lossy(ack.request().body()));
    let mut bob_bye = bob.receive("BYE").await;

    dialog.ack(Some(REANSWER)).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob_bye.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();

    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one call record: {cdrs:#?}");
    let termination = cdrs[0].termination.as_ref().expect("the record states the call's end");
    assert_eq!(termination.cause, TerminationCause::MaxDuration);

    let report = h.finish().await;
    let acks = acks_to(&report, b2bua.addr, bob.addr(), reinvite_cseq);
    assert_eq!(acks.len(), 1, "the caller's late ACK is absorbed: got {}", acks.len());
}

/// A B2BUA routing every call to `bob` under a [`CAP`] that runs from the
/// answer the caller receives.
async fn capped_b2bua(h: &Harness, bob: &Agent) -> B2buaSut {
    let (bob_ip, bob_port) = (bob.addr().ip().to_string(), bob.addr().port());
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut route = route_to(&bob_ip, bob_port);
                route.features.platform.max_duration_ms = CAP.as_millis() as i64;
                route.features.platform.max_duration_anchor = MaxDurationAnchor::Answer;
                NewCallResponse::Route(route)
            })
            .build(),
    );
    B2buaSut::builder(decision).start(h, "b2bua", &h.next_addr().to_string()).await
}

/// `sdp` answers every stream of a two-stream offer rejected: one `m=` line
/// per offered stream, each at port 0 (RFC 3264 §6).
fn assert_every_stream_rejected(sdp: &str) {
    let ports: Vec<&str> = sdp
        .lines()
        .filter_map(|l| l.strip_prefix("m="))
        .filter_map(|m| m.split_whitespace().nth(1))
        .collect();
    assert_eq!(ports, ["0", "0"], "RFC 3264 §6: every offered stream rejected:\n{sdp}");
}

/// Every ACK `sut` sent to `peer` on the INVITE numbered `cseq`, in wire order.
fn acks_to(
    report: &scenario_harness::RunReport,
    sut: SocketAddr,
    peer: SocketAddr,
    cseq: u32,
) -> Vec<Vec<u8>> {
    report
        .entries()
        .iter()
        .filter(|e| e.from == sut && e.to == peer && e.raw.starts_with(b"ACK "))
        .map(|e| e.raw.clone())
        .filter(|raw| sip_message::sniff::cseq_number(raw) == Some(cseq))
        .collect()
}

//! **The PRACK this stack owes when it CANCELs a relayed re-INVITE**, and what
//! follows it. As the CANCEL goes out, this stack PRACKs the responder's
//! reliable provisional itself (RFC 3262 §4), so:
//!
//! - the originator's own PRACK arriving afterwards names a provisional already
//!   acknowledged toward the responder: it is answered here with 200 (§3: a
//!   PRACK naming a provisional this face showed draws 2xx) and nothing goes to
//!   the responder, whose 481 to a second PRACK would land on the established
//!   dialog;
//! - this stack's own PRACK failing or timing out denies one transaction, never
//!   the established call (RFC 3261 §14.1).

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";
const ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, PRACK";

/// The responder's own `RSeq`.
const RSEQ: u32 = 4711;

/// RFC 3261 T1, §3's 64·T1 give-up bound, and Timer F (64·T1 as well).
const T1_MS: u64 = 500;
const GIVE_UP_MS: u64 = 64 * T1_MS;

/// The caller's PRACK reaches this stack after her CANCEL did: the B2BUA
/// PRACKed bob as it CANCELled, so hers is answered here and bob sees one
/// PRACK only.
#[tokio::test(start_paused = true)]
async fn a_callers_prack_after_her_reinvite_cancel_is_answered_here() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel-late-prack-caller", 1);
    let alice = h.agent("alice", "127.0.0.1:5241").await;
    let bob = h.agent("bob", "127.0.0.1:5242").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5242)
        .tune(|c| c.keepalive_interval_sec = 600)
        .start(&h, "b2bua", "127.0.0.1:5243")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", ALLOW)
        .with_header("Supported", "100rel")
        .send_cancellable()
        .await;
    let mut re_uas = bob.receive("INVITE").await;
    re_uas.respond(183, "Session Progress").reliable(RSEQ).with_sdp(REANSWER).await;
    let p183 = reinv.expect(183).await;
    let shown = stated_rseq(&p183);

    // ── alice CANCELs; this stack PRACKs bob's 183 as it CANCELs him ──
    let mut cancel = reinv.cancel().await;
    cancel.expect(200).await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    bob.receive("CANCEL").await.respond(200, "OK").await;

    // ── alice's own PRACK, crossing the 487 this stack answered her CANCEL with ──
    reinv.expect(487).await;
    let mut prack = alice_dialog
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{shown} {} INVITE", p183.cseq().seq()))
        .send()
        .await;
    prack.expect(200).await;
    re_uas.respond(487, "Request Terminated").await;
    h.advance(Duration::from_millis(2 * T1_MS)).await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "bob's 183 was acknowledged once; a second PRACK would draw his 481 (RFC 3262 §3)"
    );

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The mirror: bob re-INVITEs, alice answers reliably, bob CANCELs; his late
/// PRACK is answered here and alice sees one PRACK only.
#[tokio::test(start_paused = true)]
async fn a_callees_prack_after_his_reinvite_cancel_is_answered_here() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel-late-prack-callee", 1);
    let alice = h.agent("alice", "127.0.0.1:5244").await;
    let bob = h.agent("bob", "127.0.0.1:5245").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5245)
        .tune(|c| c.keepalive_interval_sec = 600)
        .start(&h, "b2bua", "127.0.0.1:5246")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut reinv = bob_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", ALLOW)
        .with_header("Supported", "100rel")
        .send_cancellable()
        .await;
    let mut re_uas = alice.receive("INVITE").await;
    re_uas.respond(183, "Session Progress").reliable(RSEQ).with_sdp(REANSWER).await;
    let p183 = reinv.expect(183).await;
    let shown = stated_rseq(&p183);

    let mut cancel = reinv.cancel().await;
    cancel.expect(200).await;
    alice.receive("PRACK").await.respond(200, "OK").await;
    alice.receive("CANCEL").await.respond(200, "OK").await;

    reinv.expect(487).await;
    let mut prack = bob_dialog
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{shown} {} INVITE", p183.cseq().seq()))
        .send()
        .await;
    prack.expect(200).await;
    re_uas.respond(487, "Request Terminated").await;
    h.advance(Duration::from_millis(2 * T1_MS)).await;
    assert!(
        alice.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "alice's 183 was acknowledged once (RFC 3262 §3)"
    );

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The caller lets the re-INVITE's ladder run out: §3's give-up rejects her
/// re-INVITE 504 and PRACKs bob as it CANCELs him. Her PRACK arriving after
/// that is answered here (a late PRACK still draws 2xx, §3) and bob sees one.
#[tokio::test(start_paused = true)]
async fn a_prack_after_the_reinvite_give_up_is_answered_here() {
    let h = Harness::with_transit_delay("b2bua-reinvite-giveup-late-prack", 0);
    let alice = h.agent("alice", "127.0.0.1:5247").await;
    let bob = h.agent("bob", "127.0.0.1:5248").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5248)
        .tune(|c| c.keepalive_interval_sec = 600)
        .start(&h, "b2bua", "127.0.0.1:5249")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", ALLOW)
        .with_header("Supported", "100rel")
        .send()
        .await;
    let mut re_uas = bob.receive("INVITE").await;
    re_uas.respond(183, "Session Progress").reliable(RSEQ).with_sdp(REANSWER).await;
    let p183 = reinv.expect(183).await;
    let shown = stated_rseq(&p183);

    h.advance(Duration::from_millis(GIVE_UP_MS - T1_MS + 100)).await;
    alice.drain().await;
    h.advance(Duration::from_millis(T1_MS + 100)).await;
    reinv.expect(504).await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    re_uas.respond(487, "Request Terminated").await;

    let mut prack = alice_dialog
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{shown} {} INVITE", p183.cseq().seq()))
        .send()
        .await;
    prack.expect(200).await;
    h.advance(Duration::from_millis(2 * T1_MS)).await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "bob's 183 was acknowledged once (RFC 3262 §3)"
    );

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// Bob answers the CANCEL and the 487 but never this stack's own PRACK: its
/// Timer F expiry denies that one transaction (RFC 3261 §17.1.2.2, §14.1) and
/// the established call carries on.
#[tokio::test(start_paused = true)]
async fn an_unanswered_prack_of_this_stacks_own_leaves_the_call_up() {
    let h = Harness::with_transit_delay("b2bua-reinvite-cancel-prack-timeout", 0);
    h.waive(
        WaiverScope::rule(
            "unacked-reliable-provisional",
            "alice CANCELs her renegotiation instead of PRACKing its reliable provisional — \
             the PRACK this stack owes bob in her place is the subject",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5250").await;
    let bob = h.agent("bob", "127.0.0.1:5251").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5251)
        .tune(|c| c.keepalive_interval_sec = 600)
        .start(&h, "b2bua", "127.0.0.1:5252")
        .await;
    let (alice_addr, bob_addr) = (alice.addr(), bob.addr());

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinv = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", ALLOW)
        .with_header("Supported", "100rel")
        .send_cancellable()
        .await;
    let mut re_uas = bob.receive("INVITE").await;
    re_uas.respond(183, "Session Progress").reliable(RSEQ).with_sdp(REANSWER).await;
    reinv.expect(183).await;

    let mut cancel = reinv.cancel().await;
    cancel.expect(200).await;
    let _unanswered = bob.receive("PRACK").await; // bob never answers it
    bob.receive("CANCEL").await.respond(200, "OK").await;
    re_uas.respond(487, "Request Terminated").await;
    reinv.expect(487).await;

    // ── past Timer F, the PRACK's retransmissions drained as they come ──
    for _ in 0..(GIVE_UP_MS / T1_MS + 4) {
        h.advance(Duration::from_millis(T1_MS)).await;
        assert!(
            bob.try_receive_tolerating("BYE", &["PRACK"]).await.is_none(),
            "the established call was torn down toward bob"
        );
    }
    alice.drain().await;

    let mut bye = alice_dialog.bye().await;
    bob.receive_absorbing("BYE", &["PRACK"]).await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    let byes_from_b2bua = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && (e.to == alice_addr || e.to == bob_addr))
        .filter(|e| e.raw.starts_with(b"BYE "))
        .count();
    assert_eq!(byes_from_b2bua, 1, "only alice's own BYE, relayed to bob");
}

/// The `RSeq` a reliable provisional states.
fn stated_rseq(resp: &sip_message::SipResponse) -> u32 {
    b2bua_harness::stated_by_response(resp, "RSeq")
        .and_then(|v| v.trim().parse().ok())
        .expect("a reliable provisional states its RSeq")
}

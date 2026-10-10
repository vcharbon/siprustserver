//! The peer's own CSeq sequence across the confirmation of its dialog
//! (RFC 3261 §12.2.1.1, §12.2.2).
//!
//! Each side of a dialog numbers its requests from its own sequence. The callee
//! starts its sequence with its first request, wherever that lands (the early
//! dialog included); the 2xx that confirms the dialog echoes the INVITE's CSeq,
//! which is the B2BUA's own number, never the callee's. A relayed request
//! takes the next number of the dialog it goes out on, so the far side sees
//! one unbroken run (§12.2.1.1); the B2BUA remembers the sender's last number
//! across the confirmation to recognise an old request as out of order
//! (§12.2.2, answered 500).

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness, Party, ServerTxn, WaiverScope};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";
const ALICE_REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
const BOB_REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";

/// The callee's first CSeq in a dialog it answers: m in the diagrams below.
const CALLEE_FIRST: u32 = Party::Callee.first_cseq();

fn cseq_of(txn: &ServerTxn) -> u32 {
    txn.request().cseq().seq()
}

fn from_tag_of(txn: &ServerTxn) -> String {
    txn.request().from().tag().unwrap_or_default().to_string()
}

fn assert_contiguous(seen: &[u32], what: &str) {
    let expected: Vec<u32> = (0..seen.len() as u32).map(|i| seen[0] + i).collect();
    assert_eq!(seen, expected.as_slice(), "{what}: one more than the last, every time");
}

/// What the caller was shown, per dialog: the B2BUA's From-tag on each request
/// it relayed toward the caller, with the CSeq it carried.
#[derive(Default)]
struct CallerView {
    seen: Vec<(String, u32)>,
}

impl CallerView {
    fn push(&mut self, (cseq, tag): (u32, String)) {
        self.seen.push((tag, cseq));
    }

    fn record(&mut self, txn: &ServerTxn) {
        self.push((cseq_of(txn), from_tag_of(txn)));
    }

    /// Every dialog the caller was shown carries one unbroken run.
    fn assert_each_dialog_contiguous(&self) {
        let mut tags: Vec<&str> = self.seen.iter().map(|(t, _)| t.as_str()).collect();
        tags.sort_unstable();
        tags.dedup();
        for tag in tags {
            let run: Vec<u32> =
                self.seen.iter().filter(|(t, _)| t == tag).map(|(_, c)| *c).collect();
            assert_contiguous(&run, &format!("the caller's dialog under tag {tag}"));
        }
    }
}

/// A bodiless UPDATE on `dialog`, relayed to `to`, which answers it 200.
/// The CSeq the relayed copy carried, and the From-tag it carried.
async fn relayed_update(dialog: &mut scenario_harness::Dialog, to: &Agent) -> (u32, String) {
    let mut update = dialog.request(InDialogMethod::Update, None).await;
    let mut at_peer = to.receive("UPDATE").await;
    let seen = (cseq_of(&at_peer), from_tag_of(&at_peer));
    at_peer.respond(200, "OK").await;
    update.expect(200).await;
    seen
}

/// The callee's two early UPDATEs (its CSeq m and m+1) are relayed as the
/// B2BUA's next two requests toward the caller; after the answer, its
/// re-INVITE (m+2) and BYE (m+3) continue that run toward the caller without a
/// gap.
///
/// ```text
///   callee → B2BUA                       B2BUA → caller
///   UPDATE m (early)                     UPDATE n
///   UPDATE m+1 (early)                   UPDATE n+1
///   200 (INVITE, CSeq 1: the B2BUA's)
///   re-INVITE m+2                        re-INVITE n+2
///   BYE m+3                              BYE n+3
/// ```
#[tokio::test(start_paused = true)]
async fn callee_requests_after_the_answer_continue_its_early_sequence() {
    let h = Harness::with_transit_delay("b2bua-callee-early-cseq-continues", 1);
    let alice = h.agent("alice", "127.0.0.1:7300").await;
    let bob = h.agent("bob", "127.0.0.1:7301").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7301).start(&h, "b2bua", "127.0.0.1:7302").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    let mut bob_dialog = uas.dialog();
    let mut seen = vec![
        relayed_update(&mut bob_dialog, &alice).await.0,
        relayed_update(&mut bob_dialog, &alice).await.0,
    ];
    assert_eq!(
        bob_dialog.local_cseq(),
        CALLEE_FIRST + 1,
        "the callee spent m and m+1 on its early dialog"
    );

    uas.respond(200, "OK").await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
    let mut at_alice = alice.receive("INVITE").await;
    seen.push(cseq_of(&at_alice));
    at_alice.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinvite.expect(200).await;
    bob_dialog.ack(None).await;
    alice.receive("ACK").await;

    let mut bye = bob_dialog.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    seen.push(cseq_of(&at_alice));
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    assert_contiguous(&seen, "the callee's UPDATE 1, UPDATE 2, re-INVITE 3, BYE 4 toward alice");

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The callee's sequence is its own from its first request: starting it at
/// 100 changes nothing toward the caller. The 2xx to the caller's re-INVITE in
/// between carries the B2BUA's number on the callee leg and leaves the callee's
/// own sequence where its last request put it.
///
/// ```text
///   callee → B2BUA                       B2BUA → caller
///   UPDATE 100 (early)                   UPDATE n
///   200 (INVITE)
///                  re-INVITE from alice, 200 from bob (the B2BUA's CSeq)
///   INFO 101                             INFO n+1
///   BYE 102                              BYE n+2
/// ```
#[tokio::test(start_paused = true)]
async fn the_callee_sequence_is_its_own_across_a_reinvite_answer() {
    let h = Harness::with_transit_delay("b2bua-callee-cseq-own-across-reinvite", 1);
    let alice = h.agent("alice", "127.0.0.1:7303").await;
    let bob = h.agent("bob", "127.0.0.1:7304").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7304).start(&h, "b2bua", "127.0.0.1:7305").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    let mut bob_dialog = uas.dialog();
    bob_dialog.set_local_cseq(99);
    let mut seen = vec![relayed_update(&mut bob_dialog, &alice).await.0];

    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinvite = alice_dialog.reinvite(Some(ALICE_REOFFER)).await;
    let reinvite_cseq = alice_dialog.local_cseq();
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(200, "OK").with_sdp(BOB_REANSWER).await;
    reinvite.expect(200).await;
    alice_dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    let mut info = bob_dialog.request(InDialogMethod::Info, None).await;
    let mut at_alice = alice.receive("INFO").await;
    seen.push(cseq_of(&at_alice));
    at_alice.respond(200, "OK").await;
    info.expect(200).await;

    let mut bye = bob_dialog.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    seen.push(cseq_of(&at_alice));
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    assert_contiguous(&seen, "the callee's UPDATE 100, INFO 101, BYE 102 toward alice");

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// Regression guard for the mirror direction, which confirmation never
/// renumbered: the caller's PRACK and two UPDATEs on the early dialog, then —
/// past the answer and a callee re-INVITE whose 2xx carries the B2BUA's
/// number on the caller leg — its re-INVITE and BYE reach the callee as one
/// unbroken run.
#[tokio::test(start_paused = true)]
async fn caller_requests_after_the_answer_continue_its_early_sequence() {
    let h = Harness::with_transit_delay("b2bua-caller-early-cseq-continues", 1);
    let alice = h.agent("alice", "127.0.0.1:7306").await;
    let bob = h.agent("bob", "127.0.0.1:7307").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7307).start(&h, "b2bua", "127.0.0.1:7308").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let p183 = call.expect(183).await;
    let a_tag = p183.to().tag().expect("the 183's a-facing tag").to_string();
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    let mut prack_at_bob = bob.receive("PRACK").await;
    let mut seen = vec![cseq_of(&prack_at_bob)];
    prack_at_bob.respond(200, "OK").await;
    prack.expect(200).await;

    for _ in 0..2 {
        let mut update = call.send_request(InDialogMethod::Update).with_to_tag(&a_tag).send().await;
        let mut at_bob = bob.receive("UPDATE").await;
        seen.push(cseq_of(&at_bob));
        at_bob.respond(200, "OK").await;
        update.expect(200).await;
    }

    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
    let mut at_alice = alice.receive("INVITE").await;
    at_alice.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinvite.expect(200).await;
    bob_dialog.ack(None).await;
    alice.receive("ACK").await;

    let resume = ALICE_REOFFER.replace("1 2 IN", "1 3 IN").replace("sendonly", "sendrecv");
    let mut reinvite = alice_dialog.reinvite(Some(&resume)).await;
    let reinvite_cseq = alice_dialog.local_cseq();
    let mut at_bob = bob.receive("INVITE").await;
    seen.push(cseq_of(&at_bob));
    let bob_resume = BOB_REANSWER.replace("1 2 IN", "1 3 IN").replace("recvonly", "sendrecv");
    at_bob.respond(200, "OK").with_sdp(&bob_resume).await;
    reinvite.expect(200).await;
    alice_dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    let mut at_bob = bob.receive("BYE").await;
    seen.push(cseq_of(&at_bob));
    at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    assert_contiguous(&seen, "alice's PRACK, two UPDATEs, re-INVITE and BYE toward bob");

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A forked callee: each early dialog runs its own sequence (§12.1.2), and the
/// one the 2xx confirms keeps ITS sequence. Fork 1 spent m and m+1, fork 2
/// then spent its own m; fork 1 answers, so its re-INVITE m+2 and BYE m+3
/// continue the run the caller was shown without a gap.
///
/// ```text
///   fork 1: UPDATE m, UPDATE m+1
///   fork 2: UPDATE m
///   200 (INVITE, fork 1)
///   fork 1: re-INVITE m+2, BYE m+3
/// ```
#[tokio::test(start_paused = true)]
async fn the_confirmed_fork_keeps_its_own_early_sequence() {
    let h = Harness::with_transit_delay("b2bua-confirmed-fork-keeps-its-cseq", 1);
    let alice = h.agent("alice", "127.0.0.1:7309").await;
    let bob = h.agent("bob", "127.0.0.1:7310").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7310).start(&h, "b2bua", "127.0.0.1:7311").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let a_tag1 = call.expect(180).await.to().tag().expect("fork 1's a-facing tag").to_string();
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    call.expect(180).await;

    uas.adopt_to_tag("bobfork2");
    let mut fork2 = uas.dialog();
    uas.adopt_to_tag("bobfork1");
    let mut fork1 = uas.dialog();

    let mut view = CallerView::default();
    view.push(relayed_update(&mut fork1, &alice).await);
    view.push(relayed_update(&mut fork1, &alice).await);
    view.push(relayed_update(&mut fork2, &alice).await);

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag1.as_str()), "fork 1 answers");
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinvite = fork1.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
    let mut at_alice = alice.receive("INVITE").await;
    view.record(&at_alice);
    at_alice.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinvite.expect(200).await;
    fork1.ack(None).await;
    alice.receive("ACK").await;

    let mut bye = fork1.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    view.record(&at_alice);
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    view.assert_each_dialog_contiguous();

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// §12.2.2: once the callee spent m and m+1 on its early dialog, a request it
/// sends after the answer under CSeq m is out of order — answered 500 and
/// never relayed. Its next request continues its own sequence toward the
/// caller.
#[tokio::test(start_paused = true)]
async fn an_early_number_reused_after_the_answer_is_out_of_order() {
    let h = Harness::with_transit_delay("b2bua-callee-stale-cseq-after-answer", 1);
    let alice = h.agent("alice", "127.0.0.1:7312").await;
    let bob = h.agent("bob", "127.0.0.1:7313").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7313).start(&h, "b2bua", "127.0.0.1:7314").await;
    h.waive(
        WaiverScope::rule(
            "cseq-in-dialog-order",
            "bob reuses a spent CSeq after the answer: the out-of-order request under test (RFC 3261 §12.2.2)",
        )
        .on_party("bob"),
    );

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    let mut bob_dialog = uas.dialog();
    let mut seen = vec![
        relayed_update(&mut bob_dialog, &alice).await.0,
        relayed_update(&mut bob_dialog, &alice).await.0,
    ];

    uas.respond(200, "OK").await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    bob_dialog.set_local_cseq(CALLEE_FIRST - 1);
    let mut stale = bob_dialog.request(InDialogMethod::Info, None).await;
    bob_dialog.set_local_cseq(CALLEE_FIRST + 1);
    stale.expect(500).await;
    h.advance(Duration::from_millis(500)).await;
    assert!(
        alice.try_receive_tolerating("INFO", &[]).await.is_none(),
        "an out-of-order request is not relayed"
    );

    let mut bye = bob_dialog.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    seen.push(cseq_of(&at_alice));
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    assert_contiguous(&seen, "the callee's UPDATE 1, UPDATE 2, BYE 3 toward alice");

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// §12.2.2 per fork: each early dialog of a forked callee is measured against
/// its OWN sequence. Fork 1's first UPDATE (m), after fork 2 spent m and m+1,
/// is in order and relayed; fork 1 then answers, and its reuse of m after its
/// re-INVITE (m+1) is out of order.
#[tokio::test(start_paused = true)]
async fn each_fork_is_measured_against_its_own_sequence() {
    let h = Harness::with_transit_delay("b2bua-fork-stale-cseq-own-sequence", 1);
    let alice = h.agent("alice", "127.0.0.1:7315").await;
    let bob = h.agent("bob", "127.0.0.1:7316").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7316).start(&h, "b2bua", "127.0.0.1:7317").await;
    h.waive(
        WaiverScope::rule(
            "cseq-in-dialog-order",
            "bob reuses a spent CSeq after the answer: the out-of-order request under test (RFC 3261 §12.2.2)",
        )
        .on_party("bob"),
    );

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let a_tag1 = call.expect(180).await.to().tag().expect("fork 1's a-facing tag").to_string();
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    call.expect(180).await;

    uas.adopt_to_tag("bobfork2");
    let mut fork2 = uas.dialog();
    uas.adopt_to_tag("bobfork1");
    let mut fork1 = uas.dialog();

    let mut view = CallerView::default();
    view.push(relayed_update(&mut fork2, &alice).await);
    view.push(relayed_update(&mut fork2, &alice).await);
    // In order on fork 1's own dialog, so relayed (and answered by alice).
    view.push(relayed_update(&mut fork1, &alice).await);

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag1.as_str()), "fork 1 answers");
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinvite = fork1.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
    let mut at_alice = alice.receive("INVITE").await;
    view.record(&at_alice);
    at_alice.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinvite.expect(200).await;
    fork1.ack(None).await;
    alice.receive("ACK").await;

    fork1.set_local_cseq(CALLEE_FIRST - 1);
    let mut stale = fork1.request(InDialogMethod::Info, None).await;
    fork1.set_local_cseq(CALLEE_FIRST + 1);
    stale.expect(500).await;
    h.advance(Duration::from_millis(500)).await;
    assert!(
        alice.try_receive_tolerating("INFO", &[]).await.is_none(),
        "an out-of-order request is not relayed"
    );

    let mut bye = fork1.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    view.record(&at_alice);
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    view.assert_each_dialog_contiguous();

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// §12.2.2 at the confirmation itself: fork 2 spent m and m+1 on its early
/// dialog after fork 1 had spoken; fork 2 answers, and its reuse of m right
/// after the answer is out of order against fork 2's own sequence.
#[tokio::test(start_paused = true)]
async fn the_confirmed_fork_is_measured_against_its_early_sequence() {
    let h = Harness::with_transit_delay("b2bua-confirmed-fork-stale-cseq", 1);
    let alice = h.agent("alice", "127.0.0.1:7318").await;
    let bob = h.agent("bob", "127.0.0.1:7319").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7319).start(&h, "b2bua", "127.0.0.1:7320").await;
    h.waive(
        WaiverScope::rule(
            "cseq-in-dialog-order",
            "bob reuses a spent CSeq after the answer: the out-of-order request under test (RFC 3261 §12.2.2)",
        )
        .on_party("bob"),
    );

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let a_tag2 = call.expect(180).await.to().tag().expect("fork 2's a-facing tag").to_string();

    uas.adopt_to_tag("bobfork1");
    let mut fork1 = uas.dialog();
    uas.adopt_to_tag("bobfork2");
    let mut fork2 = uas.dialog();

    let mut view = CallerView::default();
    view.push(relayed_update(&mut fork1, &alice).await);
    view.push(relayed_update(&mut fork2, &alice).await);
    view.push(relayed_update(&mut fork2, &alice).await);

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag2.as_str()), "fork 2 answers");
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    fork2.set_local_cseq(CALLEE_FIRST - 1);
    let mut stale = fork2.request(InDialogMethod::Info, None).await;
    fork2.set_local_cseq(CALLEE_FIRST + 1);
    stale.expect(500).await;
    h.advance(Duration::from_millis(500)).await;
    assert!(
        alice.try_receive_tolerating("INFO", &[]).await.is_none(),
        "an out-of-order request is not relayed"
    );

    let mut bye = fork2.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    view.record(&at_alice);
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    view.assert_each_dialog_contiguous();

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// §12.2.2 after the answer: fork 2 lost, so a request it sends once fork 1's
/// 2xx confirmed the leg names no dialog the B2BUA holds — 481, not relayed —
/// and leaves fork 1's sequence alone, so fork 1's BYE (its CSeq m) is in
/// order and ends the call.
#[tokio::test(start_paused = true)]
async fn a_losing_fork_request_after_the_answer_names_no_dialog() {
    let h = Harness::with_transit_delay("b2bua-losing-fork-request-after-answer", 1);
    let alice = h.agent("alice", "127.0.0.1:7330").await;
    let bob = h.agent("bob", "127.0.0.1:7331").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7331).start(&h, "b2bua", "127.0.0.1:7332").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    call.expect(180).await;

    uas.adopt_to_tag("bobfork2");
    let mut fork2 = uas.dialog();
    uas.adopt_to_tag("bobfork1");
    let mut fork1 = uas.dialog();

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    fork2.set_local_cseq(CALLEE_FIRST + 3);
    let mut stray = fork2.request(InDialogMethod::Info, None).await;
    stray.expect(481).await;
    h.advance(Duration::from_millis(500)).await;
    assert!(
        alice.try_receive_tolerating("INFO", &[]).await.is_none(),
        "a request on a dialog the answer abandoned is not relayed"
    );

    let mut bye = fork1.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

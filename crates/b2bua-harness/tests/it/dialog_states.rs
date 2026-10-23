//! Every dialog of a call carries the state its messages put it in (RFC 3261
//! §12.1, §15), on both faces: a callee that forks opens one early dialog per
//! To-tag, each shown to the caller under a tag of its own; the 2xx confirms
//! the fork it names and retires the others, on each face; a BYE ends the
//! dialog it names, and the BYE this stack sends on leaves the other one
//! ending until its final.
//!
//! ```text
//!   INVITE → 180(fork1) → 180(fork2)        both early, each fronted
//!          → 200(fork2) → ACK                fork2 confirmed, fork1 retired
//!   caller BYE                               her dialog ended, fork2 ending
//!          → BYE(fork2) → 200
//! ```

use b2bua_harness::{settle_until, B2buaSut};
use call::{Call, Dialog, DialogEnd, DialogState, EndingBy, PeerDialog};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const CALL_ID: &str = "dialog-states@127.0.0.1";
const FROM_TAG: &str = "alice-tag";

fn callee_fork<'a>(call: &'a Call, peer_tag: &str) -> &'a Dialog {
    call::helpers::callee_dialog(&call.b_legs[0], peer_tag).expect("the callee fork's dialog")
}

fn caller_face<'a>(call: &'a Call, our_tag: &str) -> &'a Dialog {
    call::helpers::caller_dialog(call, our_tag).expect("the caller-facing dialog")
}

#[tokio::test(start_paused = true)]
async fn each_dialog_carries_the_state_its_messages_put_it_in() {
    let h = Harness::with_transit_delay("b2bua-dialog-states", 1);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    let live = || b2bua.live_call(&call_ref);

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;

    // ── two forks ring: one early dialog per To-tag on each face ──
    uas.respond(180, "Ringing").with_to_tag("fork1").await;
    let a1 = call.expect(180).await.to().tag().expect("fork 1's face").to_string();
    uas.respond(180, "Ringing").with_to_tag("fork2").await;
    let a2 = call.expect(180).await.to().tag().expect("fork 2's face").to_string();
    assert_ne!(a1, a2, "each fork is shown under a tag of its own");
    let ringing = live().expect("the call is live");
    for (face, fork) in [(&a1, "fork1"), (&a2, "fork2")] {
        assert_eq!(callee_fork(&ringing, fork).state, DialogState::Early, "{fork} is early");
        let shown = caller_face(&ringing, face);
        assert_eq!(shown.state, DialogState::Early, "{face} is early");
        assert_eq!(
            shown.fronts,
            vec![PeerDialog { leg_id: "b-1".into(), peer_tag: fork.into() }],
            "{face} fronts {fork}",
        );
    }

    // ── fork 2 answers: confirmed on both faces, fork 1 retired on both ──
    uas.respond(200, "OK").with_to_tag("fork2").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a2.as_str()), "the answer names fork 2's face");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    settle_until(|| live().is_some_and(|c| callee_fork(&c, "fork2").awaited_ack_cseq.is_none()))
        .await;
    let answered = live().expect("the call is live");
    assert_eq!(callee_fork(&answered, "fork2").state, DialogState::Confirmed);
    assert!(callee_fork(&answered, "fork1").retired().is_some(), "the losing fork retires");
    assert_eq!(caller_face(&answered, &a2).state, DialogState::Confirmed);
    assert!(caller_face(&answered, &a1).retired().is_some(), "its face retires too");

    // ── the caller hangs up: her dialog ended, the callee's ending ──
    let mut bye = dialog.bye().await;
    let mut bye_at_bob = bob.receive("BYE").await;
    assert_eq!(bye_at_bob.request().to().tag(), Some("fork2"), "the BYE rides fork 2");
    settle_until(|| {
        live().is_some_and(|c| callee_fork(&c, "fork2").state == DialogState::Ending(EndingBy::Bye))
    })
    .await;
    let ending = live().expect("the call is live until bob answers");
    assert_eq!(caller_face(&ending, &a2).state, DialogState::Ended(DialogEnd::PeerBye));
    assert!(callee_fork(&ending, "fork1").retired().is_some(), "a retired dialog stays retired");

    bye_at_bob.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

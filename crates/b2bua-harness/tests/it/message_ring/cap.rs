//! The ring cap: a leg keeps its opening (the initial INVITE up to its first
//! final, and the ACK of a 2xx) and the last N entries after it, counting the
//! evicted; a cap of `0` (the default) records nothing.

use std::time::Duration;

use b2bua::config::CdrConfig;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

use super::MessageDirection::{Authored, Received, Relayed};
use super::{b_leg, rows, Sut, ALICE, ANSWER, BOB, OFFER};

/// Three caller INFOs past a cap of four: each leg keeps its opening (the
/// INVITE up to its 200, and the ACK) and the last four entries after it.
#[tokio::test(start_paused = true)]
async fn the_cap_keeps_the_opening_and_the_last_entries_after_it() {
    let h = Harness::new("message-ring-cap");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, CdrConfig { message_ring: 4, captured_headers: Vec::new() }).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    for _ in 0..3 {
        let mut info = dialog.request(InDialogMethod::Info, None).await;
        bob.receive("INFO").await.respond(200, "OK").await;
        info.expect(200).await;
    }
    let info_cseq = dialog.local_cseq();
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    let bye_cseq = dialog.local_cseq();
    assert_eq!(
        rows(&done.a_leg.messages.entries),
        vec![
            (Received, "INVITE", invite_cseq, None),
            (Authored, "INVITE", invite_cseq, Some(100)),
            (Relayed, "INVITE", invite_cseq, Some(180)),
            (Relayed, "INVITE", invite_cseq, Some(200)),
            (Received, "ACK", invite_cseq, None),
            (Received, "INFO", info_cseq, None),
            (Relayed, "INFO", info_cseq, Some(200)),
            (Received, "BYE", bye_cseq, None),
            (Authored, "BYE", bye_cseq, Some(200)),
        ],
        "the opening, then the last four of eight"
    );
    assert_eq!(done.a_leg.messages.dropped, 4);
    let b_methods: Vec<&str> =
        b_leg(&done).messages.entries.iter().map(|e| e.method.as_str()).collect();
    assert_eq!(b_methods, ["INVITE", "INVITE", "INVITE", "ACK", "INFO", "INFO", "BYE", "BYE"]);
    assert_eq!(
        b_leg(&done).messages.dropped,
        4,
        "eight b-leg entries after the opening, four kept"
    );
    assert_eq!(done.message_seq, 25, "the seq counts what was evicted too");
    assert_eq!(done.a_leg.messages.last_seq(), Some(23), "the 200 to the caller's BYE");

    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_ring_off_records_nothing() {
    let h = Harness::new("message-ring-off");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, CdrConfig::default()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert!(done.a_leg.messages.entries.is_empty());
    assert!(b_leg(&done).messages.entries.is_empty());
    assert_eq!(done.a_leg.messages.dropped, 0);
    assert_eq!(done.message_seq, 0);

    let _report = h.finish().await;
}

/// The liveness probe this stack originates — the in-dialog OPTIONS
/// keepalive — and its answer are not dialog history: a long call keeps its
/// setup rows under a cap the probes alone would have overrun.
#[tokio::test(start_paused = true)]
async fn keepalive_rounds_add_nothing_and_evict_nothing() {
    let h = Harness::new("message-ring-keepalive");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    // Three rounds would add twelve rows across the legs — over this cap.
    let sut = Sut::spawn(&h, CdrConfig { message_ring: 8, captured_headers: Vec::new() }).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    for _ in 0..3 {
        h.advance(Duration::from_secs(30)).await;
        alice.receive("OPTIONS").await.respond(200, "OK").await;
        bob.receive("OPTIONS").await.respond(200, "OK").await;
    }
    h.advance(Duration::from_secs(1)).await;

    let live = sut.live(&call.call_id(), dialog.local_tag());
    assert_eq!(
        rows(&live.a_leg.messages.entries),
        vec![
            (Received, "INVITE", invite_cseq, None),
            (Authored, "INVITE", invite_cseq, Some(100)),
            (Relayed, "INVITE", invite_cseq, Some(180)),
            (Relayed, "INVITE", invite_cseq, Some(200)),
            (Received, "ACK", invite_cseq, None),
        ],
        "the setup rows survive three keepalive rounds"
    );
    assert_eq!(b_leg(&live).messages.entries.len(), 4);
    assert_eq!(live.a_leg.messages.dropped, 0);
    assert_eq!(b_leg(&live).messages.dropped, 0);
    assert_eq!(live.message_seq, 9);

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let done = sut.assert_reaped().await;
    assert_eq!(done.a_leg.messages.entries.len(), 7);
    assert_eq!(done.a_leg.messages.dropped, 0);

    let _report = h.finish().await;
}

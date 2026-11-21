//! A 2xx to a re-INVITE whose client transaction took a non-2xx final is
//! dropped (RFC 6026 §7.2, §8.4: the Completed state absorbs only 3xx-6xx
//! retransmissions). The callee, broken on purpose, rejects the caller's
//! first re-INVITE 488, takes the second with a 200, then sends a 200 to the
//! first as well. The 488 drew its hop ACK; an ACK to that 200 would be a
//! second, distinct ACK for one INVITE, so none leaves, nothing reaches the
//! caller and neither leg reads it: the client transaction, still Completed
//! (Timer D), absorbs it and each copy, and counts them. The callee, never
//! ACKed, re-sends it to 64·T1 and clears the dialog with a BYE (§13.3.1.4):
//! one CDR, fully reaped.

use std::net::SocketAddr;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

use crate::common::late_2xx::{
    assert_legs_unchanged, legs, reading_every_message, repeat_on_ladder, sent_to, LADDER_MS,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE_V2: &str = "v=0\r\no=alice 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const ALICE_V3: &str = "v=0\r\no=alice 1 3 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\n";
const BOB_V2: &str = "v=0\r\no=bob 1 2 IN IP4 192.0.2.20\r\ns=-\r\nc=IN IP4 192.0.2.20\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

/// The line of `raw` starting with `name`.
fn line<'a>(raw: &'a str, name: &str) -> &'a str {
    raw.split("\r\n").find(|l| l.starts_with(name)).expect("the line")
}

/// The 200 bob would have sent the INVITE the SUT at `b2bua` sent him `nth`
/// (0 the initial one): his 200 to the `answered`th INVITE, re-addressed to
/// that INVITE's transaction (its top Via and CSeq).
fn late_200(
    h: &Harness,
    b2bua: SocketAddr,
    bob: SocketAddr,
    nth: usize,
    answered: usize,
) -> Vec<u8> {
    let entries = h.wire_entries();
    let invites: Vec<String> = entries
        .iter()
        .filter(|e| e.from == b2bua && e.to == bob && e.raw.starts_with(b"INVITE "))
        .map(|e| String::from_utf8_lossy(&e.raw).to_string())
        .collect();
    let (late, answered) = (&invites[nth], &invites[answered]);
    let ok = entries
        .iter()
        .filter(|e| e.from == bob && e.to == b2bua && e.raw.starts_with(b"SIP/2.0 200 "))
        .map(|e| String::from_utf8_lossy(&e.raw).to_string())
        .find(|r| line(r, "CSeq:") == line(answered, "CSeq:"))
        .expect("bob's 200 to the answered INVITE");
    ok.replace(line(answered, "Via:"), line(late, "Via:"))
        .replace(line(answered, "CSeq:"), line(late, "CSeq:"))
        .into_bytes()
}

#[tokio::test(start_paused = true)]
async fn a_2xx_to_a_reinvite_that_took_a_non_2xx_final_is_dropped() {
    let h = Harness::with_transit_delay("b2bua-late-first-reinvite-2xx", 1);
    h.waive(
        WaiverScope::rule(
            "single-final-per-server-txn",
            "the callee deliberately answers 200 to a re-INVITE it rejected 488 — the \
             late 2xx this test exists to drop",
        )
        .on_party("bob"),
    );
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob.addr().port())
        .tune(reading_every_message)
        .start(&h, "b2bua", &at)
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let call_ref = call::derive_call_ref("w0", &call.call_id(), dialog.local_tag());

    let mut rejected = dialog.reinvite(Some(ALICE_V2)).await;
    bob.receive("INVITE").await.respond(488, "Not Acceptable Here").await;
    rejected.expect(488).await;
    bob.receive("ACK").await;

    let mut taken = dialog.reinvite(Some(ALICE_V3)).await;
    let taken_cseq = dialog.local_cseq();
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(BOB_V2).await;
    taken.expect(200).await;
    dialog.ack_for(taken_cseq, None).await;
    bob.receive("ACK").await;

    let before = h.wire_entries().len();
    let held = legs(&b2bua.live_call(&call_ref).expect("the call is live"));
    let late = late_200(&h, b2bua.addr, bob.addr(), 1, 2);
    bob.try_send_datagram(&late, b2bua.addr).await.expect("the late 200 leaves");
    let to_give_up = repeat_on_ladder(&h, &bob, &late, b2bua.addr).await;
    assert_eq!(
        sent_to(&h, b2bua.addr, bob.addr(), before),
        Vec::<String>::new(),
        "no ACK answers the late 200 or any of its {} copies",
        LADDER_MS.len()
    );
    assert_eq!(
        sent_to(&h, b2bua.addr, alice.addr(), before),
        Vec::<String>::new(),
        "the late 200 reaches no one"
    );
    assert_legs_unchanged(&held, &b2bua.live_call(&call_ref).expect("the call is live"));
    assert_eq!(
        b2bua.txn_metrics().late_2xx_dropped(),
        1 + LADDER_MS.len() as u64,
        "the transaction, Completed past the 488, absorbs the 200 and each copy"
    );
    assert_eq!(b2bua.metrics().late_2xx_dropped_total(), 0, "none reaches the call");

    h.advance(to_give_up).await;
    let mut bye = uas.dialog().bye().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.finish().await;
}

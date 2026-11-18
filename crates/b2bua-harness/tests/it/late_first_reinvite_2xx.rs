//! A 2xx to an earlier INVITE is a copy (RFC 3261 §13.3.1.4) only of a 2xx
//! the record took. The callee, broken on purpose, rejects the caller's first
//! re-INVITE 488, takes the second with a 200, then sends a 200 to the first
//! as well: that 200 repeats no 2xx the dialog took, so it is counted on no
//! copy budget and the message ring does not mark it a repeat. The call then
//! ends with the caller's BYE: one CDR, fully reaped.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

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

/// The status lines the SUT at `b2bua` sent `to` in the wire entries past
/// `from`, requests by their request line.
fn sent_to(h: &Harness, b2bua: SocketAddr, to: SocketAddr, from: usize) -> Vec<String> {
    h.wire_entries()
        .into_iter()
        .skip(from)
        .filter(|e| e.from == b2bua && e.to == to)
        .map(|e| String::from_utf8_lossy(&e.raw).lines().next().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_2xx_to_an_earlier_reinvite_the_dialog_never_took_is_no_copy() {
    let h = Harness::with_transit_delay("b2bua-late-first-reinvite-2xx", 1);
    h.waive(
        WaiverScope::rule(
            "single-final-per-transaction",
            "the callee deliberately answers 200 to a re-INVITE it rejected 488 — the \
             late first 2xx this test exists to read",
        )
        .on_party("bob"),
    );
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let at = h.next_addr().to_string();
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob.addr().port())
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", &at)
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

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
    let late = late_200(&h, b2bua.addr, bob.addr(), 1, 2);
    bob.try_send_datagram(&late, b2bua.addr).await.expect("the late 200 leaves");
    h.advance(Duration::from_millis(5)).await;
    bob.drain().await;
    let to_bob = sent_to(&h, b2bua.addr, bob.addr(), before);
    assert_eq!(
        b2bua.metrics().copies_past_schedule_total("2xx"),
        0,
        "the late 200 copies no 2xx the dialog took: {to_bob:?}"
    );

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.cdr_records().len(), 1, "one CDR");
    h.finish().await;
}

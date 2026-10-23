//! The call record holds the INVITE this stack dialled once (ADR-0055 §2): its
//! request record keeps the image a CANCEL, an ACK or a takeover rebuilds from,
//! and no dialog the INVITE created keeps a copy of its own, ringing or
//! answered.

use b2bua_harness::{settle_until, B2buaSut};
use call::{CallBodyCodec, MsgpackCodec};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const CALL_ID: &str = "held-once@alice";
const FROM_TAG: &str = "alice-held-once";

/// How many times `needle` occurs in `haystack`.
fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack.windows(needle.len()).filter(|w| *w == needle).count()
}

#[tokio::test(start_paused = true)]
async fn the_dialled_invite_is_held_once_ringing_and_answered() {
    let h = Harness::new("b2bua-dialled-invite-held-once");
    let alice = h.agent("alice", "127.0.0.1:6201").await;
    let bob = h.agent("bob", "127.0.0.1:6202").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6202).start(&h, "b2bua", "127.0.0.1:6203").await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    let held = |stage: &str, invite: &[u8]| {
        let call = b2bua.live_call(&call_ref).expect("the call is live");
        let body = MsgpackCodec::new().encode(&call).expect("the record encodes");
        assert_eq!(
            occurrences(&body, invite),
            1,
            "{stage}: the replicated body holds the dialled INVITE once",
        );
    };

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let invite = uas.request().image().to_vec();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    held("ringing", &invite);

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    held("answered", &invite);

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

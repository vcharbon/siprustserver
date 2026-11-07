//! Promote-PEM read through `sdp()`: which 183 is promoted, and what the
//! callee's 200 changes against the description the caller was answered with.
//! A 183 promotes only when it holds a session description (RFC 3261 §13.2.1;
//! an `early-session` body is none, RFC 3959). The 200 resyncs the caller only
//! when its description moves the media: session-level values are every
//! stream's default (RFC 4566 §5), and a 200 holding no description after a
//! reliable 183 changes nothing (RFC 3262 §5: the 183 carried the answer).

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::features::RelayFirst18xStrategy;
use scenario_harness::{Harness, Inbound, WaiverScope};
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const EARLY: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const EARLY_AT_A: &str = "v=0\r\no=bob 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const FINAL_AT_B: &str = "v=0\r\no=bob 1 2 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 198.51.100.7\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const FINAL_AT_A_MEDIA_LEVEL: &str = "v=0\r\no=bob 1 2 IN IP4 192.0.2.10\r\ns=-\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\nc=IN IP4 192.0.2.10\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

async fn b2bua_pem(h: &Harness, addr: &str, dest_port: u16) -> B2buaSut {
    B2buaSut::route_all_to_with_18x("127.0.0.1", dest_port, RelayFirst18xStrategy::PromotePemTo200)
        .start(h, "b2bua", addr)
        .await
}

/// `EARLY` framed beside a text part, the way bob frames it in his 200.
fn framed(sdp: &str) -> Vec<u8> {
    format!(
        "--B\r\nContent-Type: application/sdp\r\n\r\n{sdp}\r\n\
         --B\r\nContent-Type: text/plain\r\n\r\na note about nothing\r\n--B--\r\n"
    )
    .into_bytes()
}

/// After bob's 200: the re-INVITE alice was sent, answered with her own
/// description, else `None`. Its body comes back for the assertions.
async fn resync_of(h: &Harness, alice: &scenario_harness::Agent) -> Option<String> {
    h.advance(Duration::from_secs(1)).await;
    let Some(Inbound::Request(mut txn)) = alice.take_queued_inbound().await else {
        return None;
    };
    let body = String::from_utf8_lossy(txn.request().body()).into_owned();
    txn.respond(200, "OK").with_sdp(OFFER).await;
    alice.receive("ACK").await;
    Some(body)
}

/// A promoted 183 answered by alice's ACK, then bob's 200 carrying `final_body`
/// typed `final_type`: what alice is re-offered, if anything.
async fn promoted_then_final(
    name: &str,
    ports: (u16, u16, u16),
    early: &str,
    final_body: Option<(&str, Vec<u8>)>,
) -> Option<String> {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{}", ports.0)).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{}", ports.1)).await;
    let b2bua = b2bua_pem(&h, &format!("127.0.0.1:{}", ports.2), ports.1).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(early)
        .await;
    let ok = call.expect(200).await;
    assert_eq!(ok.sdp(), Some(early.as_bytes()), "the promoted 200 answers alice");
    let mut dialog = call.ack().await;

    match final_body {
        Some((content_type, body)) => uas.respond(200, "OK").with_body(content_type, body).await,
        None => uas.respond(200, "OK").await,
    }
    bob.receive("ACK").await;
    let resync = resync_of(&h, &alice).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
    resync
}

/// A callee moving its session-level `c=` between the 183 and the 200 moves
/// every stream (RFC 4566 §5): alice hears of it by a resync re-INVITE.
#[tokio::test(start_paused = true)]
async fn a_200_moving_the_session_level_address_resyncs_the_caller() {
    let final_body = Some(("application/sdp", FINAL_AT_B.as_bytes().to_vec()));
    let resync = promoted_then_final(
        "promote-pem-session-level-c-moved",
        (5831, 5841, 5851),
        EARLY_AT_A,
        final_body,
    )
    .await;
    assert!(
        resync.is_some_and(|sdp| sdp.contains("c=IN IP4 198.51.100.7")),
        "alice keeps sending to 192.0.2.10 while bob listens at 198.51.100.7"
    );
}

/// The same address written at media level rather than session level is the
/// same media: no re-INVITE.
#[tokio::test(start_paused = true)]
async fn the_same_address_at_media_level_changes_nothing_for_the_caller() {
    let final_body = Some(("application/sdp", FINAL_AT_A_MEDIA_LEVEL.as_bytes().to_vec()));
    let resync = promoted_then_final(
        "promote-pem-address-at-media-level",
        (5832, 5842, 5852),
        EARLY_AT_A,
        final_body,
    )
    .await;
    assert_eq!(resync, None, "one address at two levels moves no media");
}

/// A 200 framing the promoted description beside a text part holds the same
/// session: no re-INVITE.
#[tokio::test(start_paused = true)]
async fn a_200_framing_the_same_description_changes_nothing_for_the_caller() {
    let final_body = Some(("multipart/mixed;boundary=B", framed(EARLY)));
    let resync =
        promoted_then_final("promote-pem-framed-200", (5833, 5843, 5853), EARLY, final_body).await;
    assert_eq!(resync, None, "the 200's description is the 183's");
}

/// A reliable 183 carried bob's answer (RFC 3262 §5), so his 200 with no body
/// changes nothing for alice: no offerless re-INVITE, and no ACK left owing
/// her an answer.
#[tokio::test(start_paused = true)]
async fn a_bodyless_200_after_a_reliable_183_changes_nothing_for_the_caller() {
    let h = Harness::with_transit_delay("promote-pem-reliable-183-bodyless-200", 1);
    let alice = h.agent("alice", "127.0.0.1:5834").await;
    let bob = h.agent("bob", "127.0.0.1:5844").await;
    let b2bua = b2bua_pem(&h, "127.0.0.1:5854", 5844).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let supported: Vec<String> =
        uas.request().raw(HeaderName::Supported).map(str::to_string).collect();
    assert!(supported.iter().any(|l| l.contains("100rel")), "bob may answer reliably");
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .reliable(1)
        .await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    // RFC 3262 §3: bob holds his 2xx until this reliable 183 is acknowledged.
    bob.receive("PRACK").await.respond(200, "OK").await;

    uas.respond(200, "OK").await;
    bob.receive("ACK").await;
    let resync = resync_of(&h, &alice).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;

    assert_eq!(resync, None, "the 183 carried the answer, so the 200 changes nothing for alice");
}

/// A 183 whose only description is disposed `early-session` holds no answer
/// to alice's offer (RFC 3959): it is relayed as a 183, not promoted, and
/// bob's 200 answers her.
#[tokio::test(start_paused = true)]
async fn an_early_session_183_is_relayed_not_promoted() {
    let h = Harness::with_transit_delay("promote-pem-early-session-183", 1);
    let alice = h.agent("alice", "127.0.0.1:5835").await;
    let bob = h.agent("bob", "127.0.0.1:5845").await;
    let b2bua = b2bua_pem(&h, "127.0.0.1:5855", 5845).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_header("Content-Disposition", "early-session")
        .with_body("application/sdp", EARLY.as_bytes().to_vec())
        .await;
    let progress = call.expect(183).await;
    assert_eq!(progress.sdp(), None, "the early session is no answer");
    uas.respond(200, "OK").with_sdp(EARLY).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.sdp(), Some(EARLY.as_bytes()), "bob's 200 answers alice");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// Alice's INVITE makes no offer, so the promoted 183's description is bob's
/// offer and her ACK answers it (RFC 3264 §4). Bob's 200 repeats the offer
/// his unreliable 183 made (RFC 3261 §13.2.1), and the ACK it draws carries
/// alice's answer.
#[tokio::test(start_paused = true)]
async fn a_delayed_offer_takes_the_callers_answer_to_the_callee() {
    let h = Harness::with_transit_delay("promote-pem-delayed-offer", 1);
    let alice = h.agent("alice", "127.0.0.1:5836").await;
    let bob = h.agent("bob", "127.0.0.1:5846").await;
    let b2bua = b2bua_pem(&h, "127.0.0.1:5856", 5846).await;

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(uas.request().sdp(), None, "a delayed offer");
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    let ok = call.expect(200).await;
    assert_eq!(ok.sdp(), Some(EARLY.as_bytes()), "bob's offer, promoted");
    let mut dialog = call.ack_with(Some(OFFER)).await;

    uas.respond(200, "OK").with_sdp(EARLY).await;
    let ack = bob.receive("ACK").await;
    let answer = ack.request().sdp().map(|sdp| String::from_utf8_lossy(sdp).into_owned());
    let resync = resync_of(&h, &alice).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;

    assert_eq!(resync, None, "bob's 200 states the promoted session");
    assert!(
        answer.as_deref().is_some_and(|sdp| sdp.contains("m=audio 10000")),
        "bob's ACK carries alice's answer: {answer:?}"
    );
}

/// The same delayed offer with bob's 200 ahead of alice's ACK: his ACK waits
/// for hers and carries her answer.
#[tokio::test(start_paused = true)]
async fn a_delayed_offer_answered_after_the_callees_200_reaches_him() {
    let h = Harness::with_transit_delay("promote-pem-delayed-offer-200-first", 1);
    let alice = h.agent("alice", "127.0.0.1:5837").await;
    let bob = h.agent("bob", "127.0.0.1:5847").await;
    let b2bua = b2bua_pem(&h, "127.0.0.1:5857", 5847).await;

    let mut call = alice.invite(&bob).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .await;
    let ok = call.expect(200).await;
    assert_eq!(ok.sdp(), Some(EARLY.as_bytes()), "bob's offer, promoted");
    uas.respond(200, "OK").with_sdp(EARLY).await;
    h.advance(Duration::from_millis(5)).await;
    let mut dialog = call.ack_with(Some(OFFER)).await;
    let ack = bob.receive("ACK").await;
    let answer = ack.request().sdp().map(|sdp| String::from_utf8_lossy(sdp).into_owned());

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;

    assert!(
        answer.as_deref().is_some_and(|sdp| sdp.contains("m=audio 10000")),
        "bob's ACK carries alice's answer: {answer:?}"
    );
}

/// A delayed offer under the promotion, alice stating `100rel` as `header`.
/// Returns whether bob's INVITE solicited a reliable provisional and the
/// answer bob was given to his 183's offer: in his PRACK, else his ACK.
async fn delayed_offer_with_100rel(
    name: &str,
    ports: (u16, u16, u16),
    header: &str,
) -> (bool, Option<String>) {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{}", ports.0)).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{}", ports.1)).await;
    let b2bua = b2bua_pem(&h, &format!("127.0.0.1:{}", ports.2), ports.1).await;

    let mut call =
        alice.invite(&bob).with_header(header, "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(uas.request().sdp(), None, "a delayed offer");
    let solicited = [HeaderName::Supported, HeaderName::Require].into_iter().any(|name| {
        uas.request().raw(name).any(|line| line.to_ascii_lowercase().contains("100rel"))
    });
    let progress = uas
        .respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY);
    if solicited {
        progress.reliable(1).await;
    } else {
        progress.await;
    }
    let ok = call.expect(200).await;
    assert_eq!(ok.sdp(), Some(EARLY.as_bytes()), "bob's offer, promoted");
    let mut dialog = call.ack_with(Some(OFFER)).await;
    let mut answered = None;
    if solicited {
        let mut prack = bob.receive("PRACK").await;
        answered = prack.request().sdp().map(|sdp| String::from_utf8_lossy(sdp).into_owned());
        prack.respond(200, "OK").await;
    }
    uas.respond(200, "OK").with_sdp(EARLY).await;
    let ack = bob.receive("ACK").await;
    let in_ack = ack.request().sdp().map(|sdp| String::from_utf8_lossy(sdp).into_owned());

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
    (solicited, answered.or(in_ack))
}

/// Alice's INVITE makes no offer, so the promotion solicits no reliable
/// provisional from bob: his 183 offers unreliably, his 200 repeats the
/// offer (RFC 3261 §13.2.1), and the ACK it draws carries alice's answer, so
/// media flows both ways.
#[tokio::test(start_paused = true)]
async fn a_delayed_offer_solicits_no_reliable_offer_from_the_callee() {
    let (solicited, answer) = delayed_offer_with_100rel(
        "promote-pem-delayed-offer-100rel",
        (5838, 5848, 5858),
        "Supported",
    )
    .await;
    assert!(
        answer.as_deref().is_some_and(|sdp| sdp.contains("m=audio 10000")),
        "bob is answered with alice's media: {answer:?}"
    );
    assert!(!solicited, "no 100rel toward bob");
}

/// Bob offers in a reliable 183 although the INVITE withheld `100rel`: the
/// audit charges bob alone, the stack PRACKs his provisional, and the call
/// ends with its BYE and its CDR.
#[tokio::test(start_paused = true)]
async fn a_callee_answering_reliably_unasked_is_charged_alone() {
    let h = Harness::with_transit_delay("promote-pem-unsolicited-reliable-offer", 1);
    h.waive(
        WaiverScope::rule(
            "reliable-needs-client-opt-in",
            "bob sends a reliable 183 to an INVITE that offered no 100rel: the outcome is the subject",
        )
        .on_party("bob"),
    );
    let alice = h.agent("alice", "127.0.0.1:5860").await;
    let bob = h.agent("bob", "127.0.0.1:5861").await;
    let b2bua = b2bua_pem(&h, "127.0.0.1:5862", 5861).await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(EARLY)
        .reliable(1)
        .await;
    call.expect(200).await;
    let mut dialog = call.ack_with(Some(OFFER)).await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    uas.respond(200, "OK").with_sdp(EARLY).await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

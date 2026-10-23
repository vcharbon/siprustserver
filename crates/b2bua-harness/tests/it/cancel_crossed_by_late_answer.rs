//! **A 2xx crossing the B2BUA's CANCEL is ACKed, then BYEd (RFC 3261 §9.1,
//! §13.2.2.4, §15).** The caller CANCELs while the callee has sent a provisional;
//! the B2BUA answers the caller (200 CANCEL, 487 INVITE) and CANCELs the callee
//! at once. The callee, which has not processed that CANCEL yet, answers the
//! INVITE 2xx: the dialog that 2xx confirms is ACKed and ended with a BYE.
//!
//! ```text
//!   alice            b2bua             bob
//!     INVITE →           INVITE →
//!                    ←   100 | 180 (T)
//!     CANCEL →
//!     ← 200 ; ← 487      CANCEL →
//!     ACK →          ←   [180 (T)] ; 200 (T)    [crossing the CANCEL]
//!                        ACK (T) →
//!                        BYE (T) →
//!                    ←   200 (BYE) ; 200 (CANCEL, late)
//! ```
//!
//! Three shapes: the dialog opens before the CANCEL (180 first), after it (a
//! bare 100, then 180 and 200 on one To-tag), or by the 2xx alone (a bare 100,
//! then 200).

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The callee's To-tag on the dialog its responses open.
const BOB_TAG: &str = "bob-answer";

/// What the callee sends before the caller's CANCEL, and what it sends after.
#[derive(Clone, Copy)]
enum Shape {
    /// 180 before the CANCEL; the 200 crosses it on the 180's dialog.
    RingingThenCrossing200,
    /// A bare 100 before the CANCEL; 180 and 200 cross it on one To-tag.
    TryingThenCrossing180And200,
    /// A bare 100 before the CANCEL; a 200 alone crosses it.
    TryingThenCrossing200,
}

async fn crossing_2xx_is_acked_then_byed(name: &str, shape: Shape, ports: [u16; 3]) {
    let h = Harness::with_transit_delay(name, 1);
    let [alice_port, bob_port, b2bua_port] = ports;
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    h.waive(
        WaiverScope::rule(
            "no-200-after-cancel",
            "bob answers 200 to an INVITE it holds a CANCEL for (RFC 3261 §9.2): the crossing under test",
        )
        .on_party("bob"),
    );
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob_port)
        .start(&h, "b2bua", &format!("127.0.0.1:{b2bua_port}"))
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    match shape {
        Shape::RingingThenCrossing200 => {
            uas.respond(180, "Ringing").with_to_tag(BOB_TAG).await;
            call.expect(180).await;
        }
        Shape::TryingThenCrossing180And200 | Shape::TryingThenCrossing200 => {
            uas.respond(100, "Trying").await;
            h.advance(Duration::from_millis(20)).await;
        }
    }

    // ── alice CANCELs: she is answered at once, and her ACK of the 487 leaves ──
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    // ── the B2BUA CANCELs bob at once: a provisional has arrived (§9.1) ──
    let mut b_cxl = bob.receive("CANCEL").await;

    // ── bob, which has not processed the CANCEL, answers the INVITE ──
    let tag = match shape {
        Shape::RingingThenCrossing200 => BOB_TAG.to_string(),
        Shape::TryingThenCrossing180And200 => {
            uas.respond(180, "Ringing").with_to_tag(BOB_TAG).await;
            BOB_TAG.to_string()
        }
        Shape::TryingThenCrossing200 => uas.answer_unrung(),
    };
    uas.respond(200, "OK").with_to_tag(&tag).with_sdp(ANSWER).await;

    // ── the B2BUA ACKs the 2xx, then BYEs the dialog it confirmed ──
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some(tag.as_str()), "the ACK names the 2xx's dialog");
    let mut bye = bob.receive("BYE").await;
    assert_eq!(bye.request().to().tag(), Some(tag.as_str()), "the BYE ends the 2xx's dialog");
    bye.respond(200, "OK").await;

    // ── bob answers the CANCEL last: the INVITE already took its final (§9.2) ──
    b_cxl.respond(200, "OK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    assert!(cdrs[0].termination.is_some(), "the cancelled call terminated");

    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_200_crossing_the_cancel_of_a_ringing_leg_is_acked_then_byed() {
    crossing_2xx_is_acked_then_byed(
        "b2bua-cancel-crossed-ringing-200",
        Shape::RingingThenCrossing200,
        [5561, 5562, 5563],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_180_and_200_crossing_the_cancel_of_a_trying_leg_are_acked_then_byed() {
    crossing_2xx_is_acked_then_byed(
        "b2bua-cancel-crossed-trying-180-200",
        Shape::TryingThenCrossing180And200,
        [5564, 5565, 5566],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_200_crossing_the_cancel_of_a_trying_leg_is_acked_then_byed() {
    crossing_2xx_is_acked_then_byed(
        "b2bua-cancel-crossed-trying-200",
        Shape::TryingThenCrossing200,
        [5567, 5568, 5569],
    )
    .await;
}

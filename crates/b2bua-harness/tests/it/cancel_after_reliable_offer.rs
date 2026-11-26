//! **A callee's 2xx this stack takes just before the caller's CANCEL is ACKed,
//! then BYEd (RFC 3261 §13.2.2.4, §15), wherever the offer rode.** The 2xx and
//! the CANCEL arrive in the same instant: the 2xx reaches the call first and is
//! relayed, but the caller's INVITE transaction already took the 487 the CANCEL
//! drew, so no caller ACK will come. This stack composes the callee's ACK on
//! its own account: bare where the ACK owes no answer (the INVITE carried the
//! offer, or a reliable provisional did and its PRACK answered it, RFC 3262 §5),
//! answering every stream rejected where the 2xx made the offer (RFC 3264 §6).
//!
//! ```text
//!   alice            b2bua             bob
//!     INVITE →           INVITE →
//!                    ←   18x (rel, offer) ; PRACK (answer) ↔ 200
//!                    ←   200 (INVITE)            [taken first]
//!     CANCEL →                                   [same instant]
//!     ← 200 ; ← 487      ACK →
//!     ACK →              BYE →
//!                    ←   200 (BYE)
//! ```

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use call::TerminationCause;
use scenario_harness::{Harness, Party};
use sip_message::generators::InDialogMethod;
use sip_message::header::RSeq;

const ALICE_SDP: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const BOB_SDP: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The callee's To-tag on the dialog its 2xx confirms.
const WINNER: &str = "fork1";

/// One reliable provisional of the callee: its fork's To-tag, status code,
/// `RSeq`, and whether it carries that fork's offer.
type Reliable = (&'static str, u16, u32, bool);

/// The message that carries the offer of the confirmed dialog (RFC 3264 §4).
#[derive(Clone, Copy)]
enum OfferIn {
    /// The caller's INVITE; the callee rings unreliably.
    Invite,
    /// The callee's reliable provisionals `rings`, each offer answered in its
    /// PRACK; `restated`: the 2xx repeats the callee's description, which
    /// makes no new offer (RFC 3262 §5).
    Reliable { rings: &'static [Reliable], restated: bool },
    /// The callee's 2xx; the callee rings unreliably.
    Final,
}

/// One early dialog: its reliable 183 carries the offer.
const ONE_FORK: &[Reliable] = &[(WINNER, 183, 7, true)];

/// Three early dialogs, interleaved; the winner rings twice, its offer on the
/// first ring.
const THREE_FORKS: &[Reliable] = &[
    ("fork2", 183, 100, true),
    ("fork3", 183, 200, true),
    ("fork3", 183, 201, false),
    (WINNER, 180, 300, true),
    ("fork2", 183, 101, false),
    (WINNER, 180, 301, false),
];

/// Bob rings, then answers in the instant alice CANCELs, the 2xx taken first:
/// bob's ACK is bare, or answers rejecting where his 2xx made the offer, the
/// BYE follows it on the 2xx's dialog, and the record ends on her CANCEL.
async fn taken_2xx_is_acked_then_byed(name: &str, offer: OfferIn, ports: [u16; 3]) {
    let h = Harness::with_transit_delay(name, 1);
    let [alice_port, bob_port, b2bua_port] = ports;
    let alice = h.agent("alice", &format!("127.0.0.1:{alice_port}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{bob_port}")).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", bob_port)
        .start(&h, "b2bua", &format!("127.0.0.1:{b2bua_port}"))
        .await;

    let invite = alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr);
    let invite = match offer {
        OfferIn::Invite => invite.with_sdp(ALICE_SDP),
        OfferIn::Reliable { .. } | OfferIn::Final => invite,
    };
    let mut call = invite.send().await;
    let mut uas = bob.receive("INVITE").await;
    match offer {
        OfferIn::Invite | OfferIn::Final => {
            uas.respond(180, "Ringing").with_to_tag(WINNER).await;
            call.expect(180).await;
        }
        OfferIn::Reliable { rings, .. } => {
            for &(tag, code, rseq, offers) in rings {
                let ring = uas
                    .respond(code, "Ringing")
                    .with_to_tag(tag)
                    .with_header("Require", "100rel")
                    .with_header("RSeq", &rseq.to_string());
                if offers {
                    ring.with_sdp(BOB_SDP).await;
                } else {
                    ring.await;
                }
                let shown = call.expect(code).await;
                let a_tag = shown.to().tag().expect("an a-facing tag").to_string();
                let rseq = shown.header::<RSeq>().expect("an RSeq").expect("a readable RSeq");
                let prack = call
                    .send_request(InDialogMethod::Prack)
                    .with_to_tag(&a_tag)
                    .with_rack(&format!("{} {} INVITE", rseq.value(), Party::Caller.first_cseq()));
                let prack = if offers { prack.with_sdp(ALICE_SDP) } else { prack };
                let mut prack = prack.send().await;
                bob.receive("PRACK").await.respond(200, "OK").await;
                prack.expect(200).await;
            }
        }
    }
    h.advance(Duration::from_millis(20)).await;

    let answer = uas.respond(200, "OK").with_to_tag(WINNER);
    match offer {
        OfferIn::Invite | OfferIn::Final | OfferIn::Reliable { restated: true, .. } => {
            answer.with_sdp(BOB_SDP).await
        }
        OfferIn::Reliable { restated: false, .. } => answer.await,
    }
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;

    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().to().tag(), Some(WINNER), "the ACK names the 2xx's dialog");
    let body = String::from_utf8_lossy(ack.request().body()).into_owned();
    match offer {
        OfferIn::Final => assert!(
            body.lines().any(|l| l.starts_with("m=audio 0 ")),
            "the ACK answers the 2xx's offer, rejecting its stream (RFC 3264 §6): {body:?}"
        ),
        OfferIn::Invite | OfferIn::Reliable { .. } => {
            assert!(body.is_empty(), "the ACK owes no answer (RFC 3264 §4): {body:?}")
        }
    }
    let mut bye = bob.receive("BYE").await;
    assert_eq!(bye.request().to().tag(), Some(WINNER), "the BYE ends the 2xx's dialog");
    bye.respond(200, "OK").await;

    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    assert_eq!(
        cdrs[0].termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteCancel),
        "the caller's CANCEL ended the call"
    );

    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_2xx_after_a_prack_answered_offer_is_acked_bare_then_byed() {
    taken_2xx_is_acked_then_byed(
        "b2bua-cancel-after-reliable-offer",
        OfferIn::Reliable { rings: ONE_FORK, restated: false },
        [5121, 5122, 5123],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_2xx_restating_a_prack_answered_offer_is_acked_bare_then_byed() {
    taken_2xx_is_acked_then_byed(
        "b2bua-cancel-after-reliable-offer-restated",
        OfferIn::Reliable { rings: ONE_FORK, restated: true },
        [5133, 5134, 5135],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_forked_2xx_after_prack_answered_offers_is_acked_bare_then_byed() {
    taken_2xx_is_acked_then_byed(
        "b2bua-cancel-after-reliable-offer-forked",
        OfferIn::Reliable { rings: THREE_FORKS, restated: false },
        [5124, 5125, 5126],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_2xx_answering_the_invite_offer_is_acked_bare_then_byed() {
    taken_2xx_is_acked_then_byed(
        "b2bua-cancel-after-invite-offer",
        OfferIn::Invite,
        [5127, 5128, 5129],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_2xx_making_the_offer_is_acked_rejecting_then_byed() {
    taken_2xx_is_acked_then_byed(
        "b2bua-cancel-after-final-offer",
        OfferIn::Final,
        [5130, 5131, 5132],
    )
    .await;
}

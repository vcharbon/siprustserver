//! A 481 answering a request relayed toward a leg this stack already ended.
//!
//! A service releases the callee mid-call while the caller stays up. The
//! caller's INFO, relayed toward the callee before the release, is answered
//! 487 in her dialog at the release (RFC 3261 §15.1.2), but its client
//! transaction toward the callee retransmits until it draws a final (§17.1.2.2).
//! The callee, whose dialog our BYE ended, answers that retransmission 481
//! (§12.2.2). A 481 ends the dialog it answers in (§12.2.1.2): that dialog is
//! already over, so the 481 ends nothing, and the caller's leg stays up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::dialogs::{DIALOG_CSEQ_SKIP, DIALOG_ENDED};
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_net::{Arrival, PreIngressAction};

use crate::common::release_callee as release;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// The caller's INFO is lost toward the callee until the release ended his
/// dialog; he answers its next retransmission 481. No BYE reaches the caller,
/// the call stays up, and her own BYE then ends it.
#[tokio::test(start_paused = true)]
async fn a_481_from_a_released_leg_leaves_the_caller_up() {
    let h = Harness::with_transit_delay("ended-leg-481", 1);
    h.waive(
        WaiverScope::rule(
            DIALOG_CSEQ_SKIP,
            "the INFO lost toward bob leaves a gap below the BYE's CSeq (RFC 3261 §12.2.2 \
             allows it)",
        )
        .on_party("bob"),
    );
    h.waive(
        WaiverScope::rule(
            DIALOG_ENDED,
            "the INFO sent before the BYE reaches bob only as a retransmission after it \
             (RFC 3261 §17.1.2.2): the lost first copy is the stimulus",
        )
        .on_party("bob"),
    );
    let alice = h.agent_auto("alice").await;
    let info_lost = Arc::new(AtomicBool::new(true));
    let losing = info_lost.clone();
    let bob = h
        .agent_with_pre_ingress(
            "bob",
            &h.next_addr().to_string(),
            Arc::new(move |arrival: &mut Arrival, _src, _load| {
                if arrival.raw().starts_with(b"INFO ") && losing.load(Ordering::SeqCst) {
                    return PreIngressAction::Drop;
                }
                PreIngressAction::Accept
            }),
        )
        .await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to(
        bob.addr().ip().to_string(),
        bob.addr().port(),
    ));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![release::service_def()])
        .tune(|c| {
            c.setup_timeout_ms = 300_000;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut info = dialog.send_request(InDialogMethod::Info).send().await;
    h.advance(Duration::from_secs(release::AT_SEC as u64) + Duration::from_millis(300)).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    info.expect(487).await;

    info_lost.store(false, Ordering::SeqCst);
    bob.receive("INFO").await.respond(481, "Call/Transaction Does Not Exist").await;
    h.advance(Duration::from_secs(1)).await;
    assert!(
        alice.try_receive_tolerating("BYE", &[]).await.is_none(),
        "a 481 from a dialog already ended ends nothing (RFC 3261 §12.2.1.2): the caller is \
         sent no BYE",
    );
    assert_eq!(b2bua.active_calls(), 1, "the caller's leg is still up");

    let mut bye = dialog.bye().await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

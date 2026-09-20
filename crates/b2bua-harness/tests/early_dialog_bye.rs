//! **A caller's BYE on an early dialog ends the call** (RFC 3261 §15, §15.1.2).
//! Once a tagged provisional has gone out, the caller holds an early dialog and
//! may BYE it. The UAS answers the BYE 200, ends the dialog, and answers the
//! still-pending INVITE 487 (§15.1.2 recommends it); this stack sends the 200
//! first. The B2BUA CANCELs the ringing callee so it stops ringing, and the
//! record is a caller release.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use call::TerminationCause;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

#[tokio::test(start_paused = true)]
async fn caller_bye_on_early_dialog_ends_the_call_and_cancels_the_callee() {
    let h = Harness::new("b2bua-early-dialog-bye");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let b2bua =
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071)))
            .start(&h, "b2bua", "127.0.0.1:5081")
            .await;

    // ── a ringing call: the 180 carries a To-tag, so alice holds an early dialog ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    let early_tag = ringing.to().tag().expect("the 180 carries an early-dialog To-tag").to_string();

    // ── alice BYEs the early dialog (§15): 200 to the BYE, 487 to the INVITE ──
    let mut bye = call.send_request(InDialogMethod::Bye).send().await;
    bye.expect(200).await;
    let rejected = call.expect(487).await; // auto-ACKed on the INVITE's branch (§17.1.1.3)
    assert_eq!(rejected.to().tag(), Some(early_tag.as_str()), "the 487 ends the early dialog");
    let seen: Vec<String> = alice.wire_view().iter().map(|e| e.start_line()).collect();
    let at = |prefix: &str| seen.iter().position(|l| l.starts_with(prefix));
    assert!(
        at("SIP/2.0 200") < at("SIP/2.0 487"),
        "200 to the BYE before 487 to the INVITE (this stack's order), got {seen:?}"
    );

    // ── the ringing callee is CANCELled and resolves 487 ──
    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    // ── fully reaped, recorded as the caller's release ──
    h.advance(Duration::from_secs(1)).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(t.by_leg.as_deref(), Some("a"));

    let _report = h.finish().await;
}

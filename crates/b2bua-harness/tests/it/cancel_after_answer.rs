//! **Late CANCEL after answer (CANCEL-2)** — a CANCEL that arrives once the call
//! is already up must NOT tear it down. RFC 3261 §9.2 with RFC 6026 §7.1: the
//! a-leg INVITE server transaction is held past its 2xx, so the transaction
//! layer answers the late CANCEL 200 itself and never surfaces a `Cancelled`;
//! the rule engine's `handle-cancel` is never reached and the bridged dialog
//! stays up.
//!
//! The transaction-layer half is pinned by sip-txn's `cancel_after_final.rs`.
//! This test adds what sip-txn cannot see: end to end through a real B2BUA the
//! late CANCEL is absorbed (alice gets 200) under the dialog's own To-tag —
//! never a tag the answer's generator fell back to — bob is NOT BYE'd, and a
//! normal BYE afterward still tears the call down cleanly (one CDR, fully
//! reaped).

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::response::is_fallback_to_tag;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

#[tokio::test]
async fn cancel_after_answer_does_not_tear_down() {
    let h = Harness::with_transit_delay("b2bua-cancel-after-answer", 1);
    // Alice's post-ACK CANCEL violates RFC 3261 §9.1 by design — the buggy
    // caller this test exists to absorb — so it is waived on alice only and
    // every B2BUA-side finding stays gated.
    h.waive(
        WaiverScope::rule(
            "no-cancel-after-final",
            "alice deliberately CANCELs after ACKing the 200 (RFC 3261 §9.1) — \
             the late-CANCEL misbehaviour whose absorption this test pins",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5076).start(&h, "b2bua", "127.0.0.1:5086").await;

    // ── establish the call (INVITE → 180 → 200 → ACK) ────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let answer = call.expect(200).await;
    let dialog_tag = answer.to().tag().expect("the 2xx binds the dialog's tag").to_string();
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice sends a LATE CANCEL (the a-leg INVITE server txn holds its 2xx) ──
    // RFC 3261 §9.2: the transaction layer answers 200 and the CANCEL never
    // reaches the B2BUA rule engine, so the established dialog is untouched.
    let mut cxl = call.cancel().await;
    let cancel_ok = cxl.expect(200).await; // 200 OK to the CANCEL — no 481, no 487
    assert_eq!(
        cancel_ok.to().tag(),
        Some(dialog_tag.as_str()),
        "the CANCEL's answer carries the tag the 2xx bound (RFC 3261 §9.2)"
    );
    assert!(!is_fallback_to_tag(&dialog_tag), "a bound tag is never the generator's fallback");
    let txn = b2bua.txn_metrics();
    assert_eq!(txn.fallback_to_tag_used(), 0, "no response of this call left under a fallback tag");
    assert_eq!(txn.to_tag_coerced(), 0, "nothing handed over a wrong tag");

    // The call must still be up: drive a real BYE and confirm the B2BUA still has
    // a confirmed b-leg to tear down (had the CANCEL torn the call down, bob would
    // have been BYE'd already and this would never arrive / would 481 here).
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    // ── one clean CDR, fully reaped: the call lived through the late CANCEL ───
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR — the call was answered then BYE'd, not cancelled");
    let cdr = &cdrs[0];
    let kinds: Vec<call::CdrEventType> = cdr.events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&call::CdrEventType::Answer), "answered: {kinds:?}");
    assert!(kinds.contains(&call::CdrEventType::Bye), "byed: {kinds:?}");
    assert!(
        !kinds.contains(&call::CdrEventType::Cancel),
        "no cancel event — late CANCEL was absorbed: {kinds:?}"
    );

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

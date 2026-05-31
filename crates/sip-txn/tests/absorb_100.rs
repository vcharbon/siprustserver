//! Port of `tests/sip/transaction-layer-100-absorb.test.ts` — `100 Trying` is
//! a hop-by-hop indicator that MUST NOT reach the app (RFC 3261 §8.1.3.4). It
//! is absorbed after nudging the client txn's state; the next `180` (with a
//! To-tag) is the first event the consumer sees. Pinned for INVITE / BYE /
//! OPTIONS CSeq methods.

mod common;
use common::*;
use sip_message::SipMessage;
use sip_txn::TransactionEvent;

const TRANSIT_MS: u64 = 5;

async fn assert_absorb_for(cseq_method: &str) {
    let mut stack = Stack::build(TRANSIT_MS, 64, 64).await;
    let call_id = format!("absorb-{cseq_method}@10.0.0.1");

    // 100 Trying — must be absorbed.
    stack
        .inject(&response_bytes(
            100,
            "Trying",
            cseq_method,
            &format!("z9hG4bK-100-{cseq_method}"),
            &call_id,
            false,
        ))
        .await;
    // 180 Ringing (with To-tag) — must be the FIRST event observed.
    stack
        .inject(&response_bytes(
            180,
            "Ringing",
            cseq_method,
            &format!("z9hG4bK-180-{cseq_method}"),
            &call_id,
            true,
        ))
        .await;

    // Auto-advance delivers + processes both packets; the owner quiesces
    // before we wake.
    elapse_ms(TRANSIT_MS * 4).await;

    let events = stack.drain_events();
    assert_eq!(events.len(), 1, "{cseq_method}: only the 180 should surface");
    match &events[0] {
        TransactionEvent::Message { message, .. } => match message.as_ref() {
            SipMessage::Response(r) => {
                assert_eq!(r.status, 180);
                assert_eq!(r.to.tag.as_deref(), Some("peer-tag"));
            }
            _ => panic!("expected a response"),
        },
        other => panic!("expected a Message event, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn absorbs_100_for_invite_cseq() {
    assert_absorb_for("INVITE").await;
}

#[tokio::test(start_paused = true)]
async fn absorbs_100_for_bye_cseq() {
    assert_absorb_for("BYE").await;
}

#[tokio::test(start_paused = true)]
async fn absorbs_100_for_options_cseq() {
    assert_absorb_for("OPTIONS").await;
}

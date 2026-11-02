//! How long an instance holds an INVITE's entry, kept for a call placed off
//! its memo-free route: Timer H once the INVITE's final passes it, the ACK
//! window of a non-2xx final (RFC 3261 §17.2.1); the full INVITE bound only
//! for an entry whose final crossed the other instance.

use std::time::Duration;

use sip_message::SipMessage;
use sip_proxy::cancel_lru::{invite_txn_key, INVITE_ENTRY_TTL_MS};
use sip_txn::timers::TIMER_H;

use super::parties::{ack_non_2xx, answer, caller_invite};
use super::rig::{winner_of, Pair, Party, Side};
use sip_message::SipRequest;

fn entry_key(invite: &SipRequest) -> String {
    let from = invite.from();
    invite_txn_key(invite.top_via(), invite.call_id().as_str(), from.tag(), invite.cseq().seq())
}

/// A spills two calls past their winners' empty buckets; one call's final
/// passes A, the other's passes B. Past Timer H A holds only the entry whose
/// final it never saw, until the INVITE bound.
#[tokio::test(start_paused = true)]
async fn an_invite_entry_lives_timer_h_past_a_final_that_passed_its_instance() {
    let pair = Pair::new().await;
    let seen = "final-seen@caller";
    let crossed = "final-crossed@caller";

    for (call_id, final_side) in [(seen, Side::A), (crossed, Side::B)] {
        let owner = winner_of(call_id).other_worker();
        pair.a.empty_bucket(winner_of(call_id));
        pair.assign(owner, final_side);
        let invite = caller_invite(call_id);
        pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
        let (at_owner, from) = pair.request(owner, "INVITE").await;
        let busy = answer(&at_owner, from, owner, 486, "Busy Here", "uas-1");
        pair.send(owner, &SipMessage::Response(busy)).await;
        let final_at_caller = pair.response(Party::Caller, 486).await;
        pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller)))
            .await;
        pair.request(owner, "ACK").await;
    }

    tokio::time::sleep(Duration::from_millis(TIMER_H + 1_000)).await;
    assert!(
        pair.a.memo.lookup(&entry_key(&caller_invite(seen))).is_none(),
        "the entry whose final passed A is gone past Timer H"
    );
    assert!(
        pair.a.memo.lookup(&entry_key(&caller_invite(crossed))).is_some(),
        "the entry whose final crossed B is held to the INVITE bound"
    );

    tokio::time::sleep(Duration::from_millis(INVITE_ENTRY_TTL_MS - TIMER_H)).await;
    assert!(pair.a.memo.lookup(&entry_key(&caller_invite(crossed))).is_none());
}

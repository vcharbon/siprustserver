//! The memo holds an INVITE's entry only when the memo-free route would miss
//! the worker the INVITE was placed on (ADR-0009 X7): a spill, a band-filtered
//! winner, a winner refusing new calls. Calls that land on their rendezvous
//! winner write no entry, so they never push a deviation out.

use sip_message::SipMessage;
use sip_proxy::cancel_lru::MemoBounds;

use super::parties::{
    ack_2xx, ack_non_2xx, answer, caller_invite, cancel_of, in_dialog, top_branch,
};
use super::rig::{calls_won_by, winner_of, InstanceOpts, Pair, Party, Side};

/// A memo of a dozen entries: what a few calls a second fill.
fn small() -> InstanceOpts {
    InstanceOpts { memo: MemoBounds { max_entries: 12, max_bytes: 1 << 20, stripes: 1 } }
}

/// `count` calls through A won by `worker`, each placed and left ringing.
async fn place_calls_won_by(pair: &Pair, worker: Party, prefix: &str, count: usize) {
    for call_id in calls_won_by(worker, prefix, count) {
        pair.send(Party::Caller, &SipMessage::Request(caller_invite(&call_id))).await;
        pair.request(worker, "INVITE").await;
    }
}

/// End a ringing call by the caller's CANCEL at `placed`.
async fn cancel_at(pair: &Pair, call_id: &str, placed: Party, at_placed: &sip_message::SipRequest) {
    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(cancel_of(&invite))).await;
    let (cancel, from) = pair.request(placed, "CANCEL").await;
    assert_eq!(top_branch(&cancel), top_branch(at_placed), "the CANCEL rides the INVITE's branch");
    let ok = answer(&cancel, from, placed, 200, "OK", "uas-1");
    pair.send(placed, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
    let terminated = answer(at_placed, from, placed, 487, "Request Terminated", "uas-1");
    pair.send(placed, &SipMessage::Response(terminated)).await;
    let final_at_caller = pair.response(Party::Caller, 487).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    pair.request(placed, "ACK").await;
}

/// A spills a call past its empty winner bucket, then places many calls on
/// their own winners: the spilled call's entry is the one entry A holds, and
/// its CANCEL reaches the worker it was placed on.
#[tokio::test(start_paused = true)]
async fn a_spilled_calls_entry_outlives_a_memo_full_of_calls_on_their_winners() {
    let pair = Pair::with(small(), InstanceOpts::default()).await;
    let call_id = "spilled-under-pressure@caller";
    let winner = winner_of(call_id);
    let placed = winner.other_worker();

    pair.a.empty_bucket(winner);
    pair.send(Party::Caller, &SipMessage::Request(caller_invite(call_id))).await;
    let (at_placed, _) = pair.request(placed, "INVITE").await;
    place_calls_won_by(&pair, placed, "on-winner", 16).await;

    cancel_at(&pair, call_id, placed, &at_placed).await;
    let memo = pair.a.memo.prometheus_text();
    assert!(memo.contains("\nsip_proxy_branch_memo_deviations_total 1\n"), "{memo}");
}

/// A reads the call's winner refusing new calls (a drain) and places it on
/// the next in rank; the winner stays alive with the calls it holds. Under
/// the same pressure the call's CANCEL reaches the worker it was placed on.
#[tokio::test(start_paused = true)]
async fn a_call_placed_past_a_draining_winner_is_cancelled_at_its_worker_under_pressure() {
    let pair = Pair::with(small(), InstanceOpts::default()).await;
    let call_id = "past-a-drain@caller";
    let winner = winner_of(call_id);
    let placed = winner.other_worker();

    pair.a.accepting(winner, false);
    pair.send(Party::Caller, &SipMessage::Request(caller_invite(call_id))).await;
    let (at_placed, _) = pair.request(placed, "INVITE").await;
    place_calls_won_by(&pair, placed, "on-winner", 16).await;

    cancel_at(&pair, call_id, placed, &at_placed).await;
}

/// A call spilled to the worker past its winner is set up; the caller's
/// re-INVITE carries the dialog's cookie, and its CANCEL, at the instance
/// that never saw the call, follows the cookie to that worker.
#[tokio::test(start_paused = true)]
async fn a_reinvites_cancel_at_the_other_instance_follows_the_cookie_to_the_spilled_worker() {
    let pair = Pair::new().await;
    let call_id = "spilled-reinvite@caller";
    let winner = winner_of(call_id);
    let placed = winner.other_worker();

    pair.a.empty_bucket(winner);
    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_placed, from) = pair.request(placed, "INVITE").await;
    let ok = answer(&at_placed, from, placed, 200, "OK", "uas-1");
    pair.send(placed, &SipMessage::Response(ok)).await;
    let answered = pair.response(Party::Caller, 200).await;
    let ack = ack_2xx(&invite, &answered);
    pair.send(Party::Caller, &SipMessage::Request(ack.clone())).await;
    pair.request(placed, "ACK").await;

    let reinvite = in_dialog(&ack, "INVITE", 2);
    pair.send(Party::Caller, &SipMessage::Request(reinvite.clone())).await;
    let (at_placed, from) = pair.request(placed, "INVITE").await;

    pair.assign(Party::Caller, Side::B);
    pair.send(Party::Caller, &SipMessage::Request(cancel_of(&reinvite))).await;
    let (cancel, cancel_from) = pair.request(placed, "CANCEL").await;
    assert_eq!(top_branch(&cancel), top_branch(&at_placed));
    let ok = answer(&cancel, cancel_from, placed, 200, "OK", "uas-1");
    pair.send(placed, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
    let terminated = answer(&at_placed, from, placed, 487, "Request Terminated", "uas-1");
    pair.send(placed, &SipMessage::Response(terminated)).await;
    let final_at_caller = pair.response(Party::Caller, 487).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&reinvite, &final_at_caller))).await;
    pair.request(placed, "ACK").await;

    pair.send(Party::Caller, &SipMessage::Request(in_dialog(&ack, "BYE", 3))).await;
    let (bye, from) = pair.request(placed, "BYE").await;
    let ok = answer(&bye, from, placed, 200, "OK", "uas-1");
    pair.send(placed, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
    pair.silent(winner).await;
}

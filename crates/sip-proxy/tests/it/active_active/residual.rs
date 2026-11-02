//! The accepted residual of ADR-0009 X7: a call placed past its rendezvous
//! winner is known only to the instance that placed it. When the caller's
//! flow then moves to the other instance, that instance sends the CANCEL to
//! the winner, which holds no such INVITE and answers 481 (RFC 3261 §9.2).
//! The call rings on at the worker that holds it until a final ends it; the
//! caller's ACK of that final reaches the worker through the instance the
//! final passed.

use sip_message::SipMessage;

use super::parties::{ack_non_2xx, answer, caller_invite, cancel_of, top_branch};
use super::rig::{winner_of, Pair, Party, Side};

#[tokio::test(start_paused = true)]
async fn a_spilled_call_meeting_a_rehash_is_cancelled_at_its_rendezvous_winner() {
    let pair = Pair::new().await;
    let call_id = "spilled-rehashed@caller";
    let winner = winner_of(call_id);
    let placed = winner.other_worker();
    pair.assign(placed, Side::B);

    pair.a.empty_bucket(winner);
    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_placed, from) = pair.request(placed, "INVITE").await;
    let ringing = answer(&at_placed, from, placed, 180, "Ringing", "uas-1");
    pair.send(placed, &SipMessage::Response(ringing)).await;
    pair.response(Party::Caller, 180).await;

    pair.assign(Party::Caller, Side::B);
    pair.send(Party::Caller, &SipMessage::Request(cancel_of(&invite))).await;
    let (cancel, cancel_from) = pair.request(winner, "CANCEL").await;
    let unknown = answer(&cancel, cancel_from, winner, 481, "Call/Transaction Does Not Exist", "w");
    pair.send(winner, &SipMessage::Response(unknown)).await;
    pair.response(Party::Caller, 481).await;

    // The accepted outcome: the worker holding the INVITE never hears the
    // CANCEL, and the call ends by that worker's own final.
    let unanswered = answer(&at_placed, from, placed, 480, "Temporarily Unavailable", "uas-1");
    pair.send(placed, &SipMessage::Response(unanswered)).await;
    let final_at_caller = pair.response(Party::Caller, 480).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    pair.request(placed, "ACK").await;
}

/// The same residual for the ACK: the spilled call's final passes A, the
/// caller's flow has moved to B, and B, holding nothing of the call, sends
/// the ACK to the rendezvous winner, which discards it. The worker holding
/// the INVITE stops retransmitting its final at Timer H.
#[tokio::test(start_paused = true)]
async fn a_spilled_calls_ack_meeting_a_rehash_reaches_its_rendezvous_winner() {
    let pair = Pair::new().await;
    let call_id = "spilled-ack-rehashed@caller";
    let winner = winner_of(call_id);
    let placed = winner.other_worker();

    pair.a.empty_bucket(winner);
    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_placed, from) = pair.request(placed, "INVITE").await;
    let busy = answer(&at_placed, from, placed, 486, "Busy Here", "uas-1");
    pair.send(placed, &SipMessage::Response(busy)).await;
    let final_at_caller = pair.response(Party::Caller, 486).await;

    pair.assign(Party::Caller, Side::B);
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    pair.request(winner, "ACK").await;
    pair.silent(placed).await;
}

/// A copy of the INVITE reaches B after the caller's flow moved, and B has no
/// token for the call's rendezvous winner: B cannot tell the copy from a new
/// call and places it on the other worker, under the INVITE's branch. That
/// worker holds no transaction on that branch and opens a second call.
#[tokio::test(start_paused = true)]
async fn an_invite_copy_meeting_a_spill_at_the_other_instance_opens_a_second_call() {
    let pair = Pair::new().await;
    let call_id = "copy-spilled@caller";
    let winner = winner_of(call_id);
    let other = winner.other_worker();

    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (first, from) = pair.request(winner, "INVITE").await;

    pair.assign(Party::Caller, Side::B);
    pair.b.empty_bucket(winner);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (copy, copy_from) = pair.request(other, "INVITE").await;
    assert_eq!(top_branch(&copy), top_branch(&first), "the copy keeps the INVITE's branch");

    // Both workers answer through B. The caller's transaction takes the
    // first final and re-sends its ACK on the second (§17.1.1.2); B relays
    // each ACK to the worker whose final it relayed last.
    pair.assign(winner, Side::B);
    pair.assign(other, Side::B);
    let busy = answer(&first, from, winner, 486, "Busy Here", "uas-1");
    pair.send(winner, &SipMessage::Response(busy)).await;
    let final_at_caller = pair.response(Party::Caller, 486).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    pair.request(winner, "ACK").await;
    let busy = answer(&copy, copy_from, other, 486, "Busy Here", "uas-2");
    pair.send(other, &SipMessage::Response(busy)).await;
    let second_final = pair.response(Party::Caller, 486).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &second_final))).await;
    pair.request(other, "ACK").await;
}

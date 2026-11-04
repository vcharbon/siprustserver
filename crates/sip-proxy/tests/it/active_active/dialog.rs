//! An established dialog's transactions across both instances, and the ACK of
//! a final an instance answered itself, at the other instance.

use sip_message::SipMessage;

use super::parties::{ack_2xx, ack_non_2xx, answer, caller_invite, in_dialog, top_branch};
use super::rig::{winner_of, Pair, Party, Side};

/// The call is set up through A with its worker's flow on B. The caller's
/// re-INVITE then crosses B, its 200 A, and its ACK B; the BYE crosses A.
/// Every request reaches the call's worker by the cookie of its route set.
#[tokio::test(start_paused = true)]
async fn a_reinvite_and_its_ack_cross_both_instances() {
    let pair = Pair::new().await;
    let call_id = "reinvite-crossing@caller";
    let owner = winner_of(call_id);
    pair.assign(owner, Side::B);

    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_owner, from) = pair.request(owner, "INVITE").await;
    let ok = answer(&at_owner, from, owner, 200, "OK", "uas-1");
    pair.send(owner, &SipMessage::Response(ok)).await;
    let answered = pair.response(Party::Caller, 200).await;
    let ack = ack_2xx(&invite, &answered);
    pair.send(Party::Caller, &SipMessage::Request(ack.clone())).await;
    pair.request(owner, "ACK").await;

    pair.assign(Party::Caller, Side::B);
    pair.assign(owner, Side::A);
    let reinvite = in_dialog(&ack, "INVITE", 2);
    pair.send(Party::Caller, &SipMessage::Request(reinvite.clone())).await;
    let (at_owner, from) = pair.request(owner, "INVITE").await;
    let ok = answer(&at_owner, from, owner, 200, "OK", "uas-1");
    pair.send(owner, &SipMessage::Response(ok)).await;
    let answered = pair.response(Party::Caller, 200).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_2xx(&reinvite, &answered))).await;
    pair.request(owner, "ACK").await;

    pair.assign(Party::Caller, Side::A);
    pair.assign(owner, Side::B);
    pair.send(Party::Caller, &SipMessage::Request(in_dialog(&ack, "BYE", 3))).await;
    let (bye, from) = pair.request(owner, "BYE").await;
    let ok = answer(&bye, from, owner, 200, "OK", "uas-1");
    pair.send(owner, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
    pair.silent(owner.other_worker()).await;
}

/// A refuses the caller's INVITE itself (no token for any worker) and the
/// caller's flow then moves to B: B recognises the ACK of A's 503 by its
/// To-tag, derived under the key both instances share, and absorbs it.
#[tokio::test(start_paused = true)]
async fn a_refusals_ack_at_the_other_instance_is_absorbed() {
    let pair = Pair::new().await;
    let invite = caller_invite("refused@caller");

    pair.a.empty_bucket(Party::W1);
    pair.a.empty_bucket(Party::W2);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let refused = pair.response(Party::Caller, 503).await;

    pair.assign(Party::Caller, Side::B);
    let ack = ack_non_2xx(&invite, &refused);
    assert_eq!(top_branch(&ack), top_branch(&invite));
    pair.send(Party::Caller, &SipMessage::Request(ack)).await;
    pair.silent(Party::W1).await;
    pair.silent(Party::W2).await;
}

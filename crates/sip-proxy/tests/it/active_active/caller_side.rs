//! The caller's transactions when its flow and a worker's reach different
//! instances: its CANCEL, its ACK of a non-2xx final, and a copy of its
//! INVITE, each at an instance whose memo holds nothing of the INVITE.

use sip_message::SipMessage;
use sip_proxy::cancel_lru::MemoBounds;

use super::parties::{ack_non_2xx, answer, caller_invite, cancel_of, top_branch};
use super::rig::{winner_of, InstanceOpts, Pair, Party, Side};

/// The caller's flow moves from A to B while the call rings, and B reads the
/// call's worker in the above-critical band: the CANCEL still reaches the
/// worker holding the INVITE, on its branch, and the call ends by the 487.
#[tokio::test(start_paused = true)]
async fn a_cancel_at_the_instance_that_never_saw_its_invite_reaches_the_invites_worker() {
    let pair = Pair::new().await;
    let call_id = "cancel-crossing@caller";
    let owner = winner_of(call_id);
    pair.assign(owner, Side::B);

    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_owner, from) = pair.request(owner, "INVITE").await;
    let ringing = answer(&at_owner, from, owner, 180, "Ringing", "uas-1");
    pair.send(owner, &SipMessage::Response(ringing)).await;
    pair.response(Party::Caller, 180).await;

    pair.assign(Party::Caller, Side::B);
    pair.b.read_critical(owner);
    pair.send(Party::Caller, &SipMessage::Request(cancel_of(&invite))).await;
    let (cancel, from) = pair.request(owner, "CANCEL").await;
    assert_eq!(top_branch(&cancel), top_branch(&at_owner), "the CANCEL rides the INVITE's branch");

    let ok = answer(&cancel, from, owner, 200, "OK", "uas-1");
    pair.send(owner, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
    let terminated = answer(&at_owner, from, owner, 487, "Request Terminated", "uas-1");
    pair.send(owner, &SipMessage::Response(terminated.clone())).await;
    let final_at_caller = pair.response(Party::Caller, 487).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    let (ack, _) = pair.request(owner, "ACK").await;
    assert_eq!(top_branch(&ack), top_branch(&at_owner), "the ACK ends the INVITE's transaction");
    pair.silent(owner.other_worker()).await;
}

/// The instance the caller's CANCEL reaches has no token left for any worker:
/// the CANCEL belongs to a call already admitted, so it is neither refused
/// nor moved to another worker.
#[tokio::test(start_paused = true)]
async fn an_empty_bucket_never_turns_a_cancel_away() {
    let pair = Pair::new().await;
    let call_id = "cancel-empty-bucket@caller";
    let owner = winner_of(call_id);

    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_owner, from) = pair.request(owner, "INVITE").await;

    pair.assign(Party::Caller, Side::B);
    pair.b.empty_bucket(Party::W1);
    pair.b.empty_bucket(Party::W2);
    pair.send(Party::Caller, &SipMessage::Request(cancel_of(&invite))).await;
    let (cancel, _) = pair.request(owner, "CANCEL").await;
    assert_eq!(top_branch(&cancel), top_branch(&at_owner));

    let ok = answer(&cancel, from, owner, 200, "OK", "uas-1");
    pair.send(owner, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
    let terminated = answer(&at_owner, from, owner, 487, "Request Terminated", "uas-1");
    pair.send(owner, &SipMessage::Response(terminated)).await;
    let final_at_caller = pair.response(Party::Caller, 487).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    pair.request(owner, "ACK").await;
    pair.silent(owner.other_worker()).await;
}

/// A placed the call past its rendezvous winner (an empty bucket), the worker
/// it placed it on answers through B, and the caller's ACK of that final
/// reaches A, which never saw the final: the ACK still ends the INVITE's
/// transaction at the worker that sent the final.
#[tokio::test(start_paused = true)]
async fn a_caller_ack_of_a_final_that_crossed_reaches_the_worker_that_sent_it() {
    let pair = Pair::new().await;
    let call_id = "ack-crossing@caller";
    let winner = winner_of(call_id);
    let placed = winner.other_worker();
    pair.assign(placed, Side::B);

    pair.a.empty_bucket(winner);
    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_placed, from) = pair.request(placed, "INVITE").await;
    let busy = answer(&at_placed, from, placed, 486, "Busy Here", "uas-1");
    pair.send(placed, &SipMessage::Response(busy)).await;
    let final_at_caller = pair.response(Party::Caller, 486).await;

    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    let (ack, _) = pair.request(placed, "ACK").await;
    assert_eq!(top_branch(&ack), top_branch(&at_placed));
    pair.silent(winner).await;
}

/// The worker's final passes A and the caller's flow then moves to B: B holds
/// nothing of the call, and the ACK reaches the worker by the call's
/// rendezvous rank.
#[tokio::test(start_paused = true)]
async fn a_caller_ack_at_an_instance_holding_nothing_of_the_call_reaches_its_worker() {
    let pair = Pair::new().await;
    let call_id = "ack-empty-memo@caller";
    let owner = winner_of(call_id);

    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_owner, from) = pair.request(owner, "INVITE").await;
    let busy = answer(&at_owner, from, owner, 486, "Busy Here", "uas-1");
    pair.send(owner, &SipMessage::Response(busy)).await;
    let final_at_caller = pair.response(Party::Caller, 486).await;

    pair.assign(Party::Caller, Side::B);
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    let (ack, _) = pair.request(owner, "ACK").await;
    assert_eq!(top_branch(&ack), top_branch(&at_owner));
    pair.silent(owner.other_worker()).await;
}

/// A copy of the caller's INVITE reaches B, which never saw the first: B
/// cannot tell it from a new call, and places it on the call's rendezvous
/// winner under the INVITE's own branch, where the worker's server
/// transaction absorbs it (RFC 3261 §17.2.3).
#[tokio::test(start_paused = true)]
async fn an_invite_copy_at_the_instance_that_never_saw_it_reaches_the_same_worker() {
    let pair = Pair::new().await;
    let call_id = "invite-copy@caller";
    let owner = winner_of(call_id);

    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (first, from) = pair.request(owner, "INVITE").await;

    pair.assign(Party::Caller, Side::B);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (copy, _) = pair.request(owner, "INVITE").await;
    assert_eq!(top_branch(&copy), top_branch(&first), "one server transaction absorbs both");
    pair.silent(owner.other_worker()).await;

    let busy = answer(&first, from, owner, 486, "Busy Here", "uas-1");
    pair.send(owner, &SipMessage::Response(busy)).await;
    let final_at_caller = pair.response(Party::Caller, 486).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    pair.request(owner, "ACK").await;
}

/// A rings long while its memo, at its bound, forgets the call's INVITE entry
/// for newer calls, and its buckets run dry: the caller's CANCEL, which A no
/// longer correlates, still reaches the worker holding the INVITE.
#[tokio::test(start_paused = true)]
async fn a_long_ringing_call_whose_entry_was_evicted_is_cancelled_at_its_worker() {
    let small = MemoBounds { max_entries: 12, max_bytes: 1 << 20, stripes: 1 };
    let pair = Pair::with(InstanceOpts { memo: small }, InstanceOpts::default()).await;
    let call_id = "long-ring@caller";
    let owner = winner_of(call_id);

    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_owner, from) = pair.request(owner, "INVITE").await;
    let ringing = answer(&at_owner, from, owner, 180, "Ringing", "uas-1");
    pair.send(owner, &SipMessage::Response(ringing)).await;
    pair.response(Party::Caller, 180).await;

    for n in 0..16 {
        let newer = format!("newer-{n}@caller");
        pair.send(Party::Caller, &SipMessage::Request(caller_invite(&newer))).await;
        pair.request(winner_of(&newer), "INVITE").await;
    }
    assert!(pair.a.memo.evicted() > 0, "the memo forgot entries at its bound");

    pair.a.empty_bucket(Party::W1);
    pair.a.empty_bucket(Party::W2);
    pair.send(Party::Caller, &SipMessage::Request(cancel_of(&invite))).await;
    let (cancel, _) = pair.request(owner, "CANCEL").await;
    assert_eq!(top_branch(&cancel), top_branch(&at_owner));

    let ok = answer(&cancel, from, owner, 200, "OK", "uas-1");
    pair.send(owner, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
    let terminated = answer(&at_owner, from, owner, 487, "Request Terminated", "uas-1");
    pair.send(owner, &SipMessage::Response(terminated)).await;
    let final_at_caller = pair.response(Party::Caller, 487).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    pair.request(owner, "ACK").await;
}

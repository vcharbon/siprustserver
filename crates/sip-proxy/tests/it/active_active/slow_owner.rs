//! A transaction held by a worker read `Slow` (ADR-0069) is followed to it:
//! a CANCEL no memo places ranks the workers serving dialogs, `Slow` ones
//! included, and a CANCEL whose memo names a `Slow` backup stays on it while
//! the call's primary serves again.
//!
//! Timing axes: none; each health turn is set on the instances' views before
//! the request it bears on, and every party answers at once.

use sip_message::SipMessage;
use sip_proxy::registry::WorkerHealth;

use super::off_rank::cancel_reaches;
use super::parties::{
    ack_2xx, ack_non_2xx, answer, caller_invite, cancel_of, emergency_invite, in_dialog, top_branch,
};
use super::rig::{calls_won_by, winner_of, Pair, Party, Side};

/// A call A placed on its winner while both workers were `Alive`; the winner
/// then reads `Slow` on both instances. The caller's CANCEL at B, which holds
/// no entry, reaches the winner holding the INVITE, not the `Alive` worker.
#[tokio::test(start_paused = true)]
async fn a_cancel_no_memo_places_reaches_the_slow_worker_holding_the_invite() {
    let pair = Pair::new().await;
    let call_id = calls_won_by(Party::W1, "slow-holder", 1).remove(0);
    pair.send(Party::Caller, &SipMessage::Request(caller_invite(&call_id))).await;
    let (at_winner, _) = pair.request(Party::W1, "INVITE").await;

    pair.a.read(Party::W1, WorkerHealth::Slow);
    pair.b.read(Party::W1, WorkerHealth::Slow);
    pair.assign(Party::Caller, Side::B);
    cancel_reaches(&pair, &call_id, Party::W1, &at_winner).await;
    pair.silent(Party::W2).await;
}

/// Every worker `Slow` on both instances: A places an emergency INVITE on
/// the winner among them, and the caller's CANCEL at B, which holds no entry,
/// reaches it.
#[tokio::test(start_paused = true)]
async fn the_cancel_of_an_emergency_call_on_a_slow_worker_reaches_it_with_no_memo() {
    let pair = Pair::new().await;
    let call_id = "slow-emergency@caller";
    let winner = winner_of(call_id);
    for worker in [Party::W1, Party::W2] {
        pair.a.read(worker, WorkerHealth::Slow);
        pair.b.read(worker, WorkerHealth::Slow);
    }
    pair.send(Party::Caller, &SipMessage::Request(emergency_invite(call_id))).await;
    let (at_winner, _) = pair.request(winner, "INVITE").await;

    pair.assign(Party::Caller, Side::B);
    cancel_reaches(&pair, call_id, winner, &at_winner).await;
    pair.silent(winner.other_worker()).await;
}

/// An established call whose primary A reads `Dead`: the caller's re-INVITE
/// goes to the backup. The backup then reads `Slow` and the primary `Alive`
/// again: the re-INVITE's CANCEL at A follows its entry to the backup that
/// holds the transaction, and so does the ACK of its 487; the BYE goes to the
/// primary.
#[tokio::test(start_paused = true)]
async fn a_reinvites_cancel_stays_on_its_slow_backup_while_the_primary_serves_again() {
    let pair = Pair::new().await;
    let call_id = "slow-backup-reinvite@caller";
    let primary = winner_of(call_id);
    let backup = primary.other_worker();

    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(invite.clone())).await;
    let (at_primary, from) = pair.request(primary, "INVITE").await;
    let ok = answer(&at_primary, from, primary, 200, "OK", "uas-1");
    pair.send(primary, &SipMessage::Response(ok)).await;
    let answered = pair.response(Party::Caller, 200).await;
    let ack = ack_2xx(&invite, &answered);
    pair.send(Party::Caller, &SipMessage::Request(ack.clone())).await;
    pair.request(primary, "ACK").await;

    pair.a.read(primary, WorkerHealth::Dead);
    let reinvite = in_dialog(&ack, "INVITE", 2);
    pair.send(Party::Caller, &SipMessage::Request(reinvite.clone())).await;
    let (at_backup, from) = pair.request(backup, "INVITE").await;
    pair.a.read(backup, WorkerHealth::Slow);
    pair.a.read(primary, WorkerHealth::Alive);

    pair.send(Party::Caller, &SipMessage::Request(cancel_of(&reinvite))).await;
    let (cancel, cancel_from) = pair.request(backup, "CANCEL").await;
    assert_eq!(top_branch(&cancel), top_branch(&at_backup));
    let ok = answer(&cancel, cancel_from, backup, 200, "OK", "uas-1");
    pair.send(backup, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
    let terminated = answer(&at_backup, from, backup, 487, "Request Terminated", "uas-1");
    pair.send(backup, &SipMessage::Response(terminated)).await;
    let final_at_caller = pair.response(Party::Caller, 487).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&reinvite, &final_at_caller))).await;
    pair.request(backup, "ACK").await;

    pair.send(Party::Caller, &SipMessage::Request(in_dialog(&ack, "BYE", 3))).await;
    let (bye, from) = pair.request(primary, "BYE").await;
    let ok = answer(&bye, from, primary, 200, "OK", "uas-1");
    pair.send(primary, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
}

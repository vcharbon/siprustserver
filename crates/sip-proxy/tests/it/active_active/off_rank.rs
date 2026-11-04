//! A call placed away from its rendezvous winner among the alive workers (a
//! spill past an empty bucket, a winner refusing new calls, a winner that was
//! away when the call came): the instance that placed it holds its entry, so
//! the caller's CANCEL there reaches the worker holding the call. At the
//! other instance, after the caller's flow moved, the CANCEL takes the
//! memo-free route to the winner, which answers 481 (ADR-0009 X7, the
//! accepted residual).

use sip_message::{SipMessage, SipRequest};
use sip_proxy::registry::WorkerHealth;

use super::parties::{
    ack_2xx, ack_non_2xx, answer, caller_invite, cancel_of, in_dialog, top_branch,
};
use super::rig::{calls_won_by, winner_of, Pair, Party, Side};

/// The caller's CANCEL of the ringing `call_id`, reaching `placed`, which
/// ends the call: 200, 487, ACK.
async fn cancel_reaches(pair: &Pair, call_id: &str, placed: Party, at_placed: &SipRequest) {
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

/// The caller's CANCEL of the ringing `call_id`, reaching the call's winner
/// `winner`, which holds no such INVITE and answers 481 (§9.2); the call then
/// ends by the 480 of `placed`, which never heard the CANCEL.
async fn cancel_draws_481(
    pair: &Pair,
    call_id: &str,
    winner: Party,
    placed: Party,
    at_placed: &SipRequest,
    from_placed: std::net::SocketAddr,
) {
    let invite = caller_invite(call_id);
    pair.send(Party::Caller, &SipMessage::Request(cancel_of(&invite))).await;
    let (cancel, from) = pair.request(winner, "CANCEL").await;
    let unknown = answer(&cancel, from, winner, 481, "Call/Transaction Does Not Exist", "w");
    pair.send(winner, &SipMessage::Response(unknown)).await;
    pair.response(Party::Caller, 481).await;
    let unanswered =
        answer(at_placed, from_placed, placed, 480, "Temporarily Unavailable", "uas-1");
    pair.send(placed, &SipMessage::Response(unanswered)).await;
    let final_at_caller = pair.response(Party::Caller, 480).await;
    pair.send(Party::Caller, &SipMessage::Request(ack_non_2xx(&invite, &final_at_caller))).await;
    pair.request(placed, "ACK").await;
}

/// How a cell makes A place a call won by `winner` on the other worker.
#[derive(Clone, Copy, Debug)]
enum OffRank {
    /// `winner`'s AIMD bucket at A is empty: the call spills.
    Spill,
    /// A reads `winner` refusing new calls (a drain), alive with its calls.
    Drain,
}

impl OffRank {
    fn apply(self, pair: &Pair, winner: Party) {
        match self {
            OffRank::Spill => pair.a.empty_bucket(winner),
            OffRank::Drain => pair.a.accepting(winner, false),
        }
    }
}

/// Two calls won by the same worker, both placed by A on the other one. The
/// first is CANCELed at A: the CANCEL reaches the worker holding it. The
/// caller's flow then moves to B, which holds nothing of the second: its
/// CANCEL reaches the winner and draws 481, the accepted residual.
async fn placed_off_rank(how: OffRank) {
    let pair = Pair::new().await;
    let [kept, moved] = <[String; 2]>::try_from(calls_won_by(Party::W1, &format!("{how:?}"), 2))
        .expect("two calls");
    let (winner, placed) = (Party::W1, Party::W2);
    pair.assign(placed, Side::B);

    how.apply(&pair, winner);
    let mut at_placed = Vec::new();
    for call_id in [&kept, &moved] {
        pair.send(Party::Caller, &SipMessage::Request(caller_invite(call_id))).await;
        at_placed.push(pair.request(placed, "INVITE").await);
    }

    cancel_reaches(&pair, &kept, placed, &at_placed[0].0).await;
    pair.assign(Party::Caller, Side::B);
    cancel_draws_481(&pair, &moved, winner, placed, &at_placed[1].0, at_placed[1].1).await;
}

#[tokio::test(start_paused = true)]
async fn a_spilled_calls_cancel_reaches_its_worker_at_the_placing_instance_only() {
    placed_off_rank(OffRank::Spill).await;
}

#[tokio::test(start_paused = true)]
async fn a_call_placed_past_a_draining_winner_is_cancelled_at_its_worker_by_the_placing_instance() {
    placed_off_rank(OffRank::Drain).await;
}

/// A rolling restart: A reads W1 away when a call it wins arrives, and places
/// it on W2; W1 comes back alive while the call rings. A's entry still names
/// W2, so the caller's CANCEL at A reaches the worker holding the call, not
/// the returned winner.
#[tokio::test(start_paused = true)]
async fn a_call_placed_while_its_winner_was_away_is_cancelled_at_its_worker_after_the_return() {
    let pair = Pair::new().await;
    let call_id = calls_won_by(Party::W1, "rolling", 1).remove(0);

    pair.a.read(Party::W1, WorkerHealth::Dead);
    pair.send(Party::Caller, &SipMessage::Request(caller_invite(&call_id))).await;
    let (at_placed, _) = pair.request(Party::W2, "INVITE").await;
    pair.a.read(Party::W1, WorkerHealth::Alive);

    cancel_reaches(&pair, &call_id, Party::W2, &at_placed).await;
    pair.silent(Party::W1).await;
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

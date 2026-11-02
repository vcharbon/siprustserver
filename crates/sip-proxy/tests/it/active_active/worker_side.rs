//! A worker's dialling transaction when its flow and the callee's reach
//! different instances: its CANCEL and its ACK of a non-2xx final, each at an
//! instance whose memo holds nothing of what they need.

use sip_message::SipMessage;

use super::parties::{ack_non_2xx, answer, cancel_of, top_branch, worker_invite};
use super::rig::{Pair, Party, Side};

/// W1 dials through B, the callee answers through A, and W1's flow then moves
/// to A: its CANCEL reaches the callee on the INVITE's branch.
#[tokio::test(start_paused = true)]
async fn a_workers_cancel_at_the_instance_that_never_saw_its_invite_reaches_the_callee() {
    let pair = Pair::new().await;
    pair.assign(Party::W1, Side::B);

    let invite = worker_invite(Party::W1, "leg-cancel@w1");
    pair.send(Party::W1, &SipMessage::Request(invite.clone())).await;
    let (at_callee, from) = pair.request(Party::Callee, "INVITE").await;
    let ringing = answer(&at_callee, from, Party::Callee, 180, "Ringing", "callee-1");
    pair.send(Party::Callee, &SipMessage::Response(ringing)).await;
    pair.response(Party::W1, 180).await;

    pair.assign(Party::W1, Side::A);
    pair.send(Party::W1, &SipMessage::Request(cancel_of(&invite))).await;
    let (cancel, cancel_from) = pair.request(Party::Callee, "CANCEL").await;
    assert_eq!(top_branch(&cancel), top_branch(&at_callee));

    let ok = answer(&cancel, cancel_from, Party::Callee, 200, "OK", "callee-1");
    pair.send(Party::Callee, &SipMessage::Response(ok)).await;
    pair.response(Party::W1, 200).await;
    let terminated = answer(&at_callee, from, Party::Callee, 487, "Request Terminated", "callee-1");
    pair.send(Party::Callee, &SipMessage::Response(terminated)).await;
    let final_at_w1 = pair.response(Party::W1, 487).await;
    pair.send(Party::W1, &SipMessage::Request(ack_non_2xx(&invite, &final_at_w1))).await;
    let (ack, _) = pair.request(Party::Callee, "ACK").await;
    assert_eq!(top_branch(&ack), top_branch(&at_callee));
}

/// W1 dials through B and the callee's 486 comes back through A: B, which the
/// ACK reaches, never saw the final, and the ACK still ends the INVITE's
/// transaction at the callee.
#[tokio::test(start_paused = true)]
async fn a_workers_ack_of_a_final_that_crossed_reaches_the_callee() {
    let pair = Pair::new().await;
    pair.assign(Party::W1, Side::B);

    let invite = worker_invite(Party::W1, "leg-ack@w1");
    pair.send(Party::W1, &SipMessage::Request(invite.clone())).await;
    let (at_callee, from) = pair.request(Party::Callee, "INVITE").await;
    let busy = answer(&at_callee, from, Party::Callee, 486, "Busy Here", "callee-1");
    pair.send(Party::Callee, &SipMessage::Response(busy)).await;
    let final_at_w1 = pair.response(Party::W1, 486).await;

    pair.send(Party::W1, &SipMessage::Request(ack_non_2xx(&invite, &final_at_w1))).await;
    let (ack, _) = pair.request(Party::Callee, "ACK").await;
    assert_eq!(top_branch(&ack), top_branch(&at_callee));
}

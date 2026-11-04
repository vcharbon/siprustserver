//! Two instances holding different health for one call's primary (ADR-0009
//! X7): each routes the call's traffic it receives by its own view. While A
//! reads the primary dead and B reads it alive, the caller's requests and
//! the caller's responses to the primary's requests, which reach A, go to
//! the call's backup; the primary's own requests, which reach B, go out as
//! they came. The window closes when A reads the primary alive again.

use sip_message::SipMessage;
use sip_proxy::registry::WorkerHealth;

use super::parties::{ack_2xx, answer, caller_invite, in_dialog, uas_in_dialog};
use super::rig::{winner_of, Pair, Party, Side};

#[tokio::test(start_paused = true)]
async fn each_instance_routes_a_call_by_its_own_view_of_the_primary() {
    let pair = Pair::new().await;
    let call_id = "split-view@caller";
    let primary = winner_of(call_id);
    let backup = primary.other_worker();
    pair.assign(primary, Side::B);
    pair.assign(backup, Side::B);

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

    // A: the caller's request goes to the backup.
    pair.send(Party::Caller, &SipMessage::Request(in_dialog(&ack, "INFO", 2))).await;
    let (info, info_from) = pair.request(backup, "INFO").await;
    let ok = answer(&info, info_from, backup, 200, "OK", "uas-1");
    pair.send(backup, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;

    // B: the primary's own request reaches the caller.
    let keepalive = uas_in_dialog(&at_primary, primary, "uas-1", "OPTIONS", 1);
    pair.send(primary, &SipMessage::Request(keepalive)).await;
    let (at_caller, at_caller_from) = pair.request(Party::Caller, "OPTIONS").await;

    // A: the caller's answer to it goes to the backup.
    let ok = answer(&at_caller, at_caller_from, Party::Caller, 200, "OK", "caller-split-view");
    pair.send(Party::Caller, &SipMessage::Response(ok)).await;
    pair.response(backup, 200).await;

    // A reads the primary alive again: the call's traffic converges on it.
    pair.a.read(primary, WorkerHealth::Alive);
    pair.send(Party::Caller, &SipMessage::Request(in_dialog(&ack, "BYE", 3))).await;
    let (bye, bye_from) = pair.request(primary, "BYE").await;
    let ok = answer(&bye, bye_from, primary, 200, "OK", "uas-1");
    pair.send(primary, &SipMessage::Response(ok)).await;
    pair.response(Party::Caller, 200).await;
}

//! The INVITE transactions a leg's ladder holds open (`close::transactions`).

use super::*;

/// A non-2xx final this leg sent to an INVITE holds its server transaction
/// in Completed until the peer's ACK (RFC 3261 §17.2.1, §17.1.1.3): listed
/// from its first emission — a retransmission re-opens no clock — until
/// the ACK naming its CSeq. A 2xx is the peer's own request to acknowledge
/// (§13.2.2.4) and a non-INVITE final is never ACKed: neither is listed.
#[test]
fn a_non_2xx_invite_final_this_leg_sent_awaits_the_peer_s_ack() {
    let invite = (Dir::In, INVITE.to_string(), false);
    let ok = (Dir::Out, response(200, "1 INVITE"), false);
    let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""), false);
    let reinvite = (Dir::In, taken("INVITE", "2 INVITE", Some("b1"), ""), false);
    let bye = (Dir::In, taken("BYE", "3 BYE", Some("b2"), ""), false);
    let refused = (Dir::Out, response(481, "3 BYE").replace("tag=b1", "tag=b2"), false);
    let terminated = (Dir::Out, response(487, "2 INVITE"), false);
    let again = (Dir::Out, response(487, "2 INVITE"), true);
    let ladder = |entries: &[(Dir, String, bool)]| {
        unacked_finals(&record(entries).legs().remove("A").expect("the leg was recorded"))
    };

    assert_eq!(ladder(&[invite.clone(), ok.clone(), ack.clone()]), [], "a 2xx owes nothing");
    assert_eq!(
        ladder(&[invite.clone(), ok.clone(), ack.clone(), bye.clone(), refused.clone()]),
        [],
        "a non-INVITE final owes nothing"
    );
    let sent = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite.clone(),
        bye.clone(),
        refused.clone(),
        terminated.clone(),
        again.clone(),
    ]);
    assert_eq!(sent, [UnackedFinal { cseq: 2, status: 487, sent_at_us: 6 }]);
    let acked = (Dir::In, taken("ACK", "2 ACK", Some("b1"), ""), false);
    let discharged = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite.clone(),
        bye.clone(),
        refused.clone(),
        terminated.clone(),
        again.clone(),
        acked.clone(),
    ]);
    assert_eq!(discharged, [], "the peer's ACK ended the transaction");
    // A second final on the transaction the 487 already ended answers
    // nothing the peer holds open: it neither re-opens the CSeq nor moves
    // the first final's clock, before the ACK or after it.
    let late = (Dir::Out, response(408, "2 INVITE"), false);
    let before_ack = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite.clone(),
        bye.clone(),
        refused.clone(),
        terminated.clone(),
        again.clone(),
        late.clone(),
    ]);
    assert_eq!(before_ack, [UnackedFinal { cseq: 2, status: 487, sent_at_us: 6 }]);
    let then_acked = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite.clone(),
        bye.clone(),
        refused.clone(),
        terminated.clone(),
        again.clone(),
        late.clone(),
        acked.clone(),
    ]);
    assert_eq!(then_acked, [], "the ACK still discharges the 487");
    let after_ack =
        ladder(&[invite, ok, ack, reinvite, bye, refused, terminated, again, acked, late]);
    assert_eq!(after_ack, [], "a final on an ended transaction is owed no ACK");
}

/// Every INVITE this leg sent awaits its final on its own CSeq (RFC 3261
/// §17.1.1.2): Calling from its first emission, Proceeding once a provisional
/// on that CSeq arrives, gone at any final — so a re-INVITE is awaited while
/// the initial INVITE its 2xx answered is not.
#[test]
fn an_invite_this_leg_sent_awaits_its_final_on_its_own_cseq() {
    let invite = (Dir::Out, INVITE.to_string(), false);
    let ok = (Dir::In, response(200, "1 INVITE"), false);
    let ack = (Dir::Out, INVITE.replace("INVITE", "ACK"), false);
    let reinvite = (Dir::Out, INVITE.replace("CSeq: 1 INVITE", "CSeq: 2 INVITE"), false);
    let trying = (Dir::In, response(100, "2 INVITE"), false);
    let rejected = (Dir::In, response(491, "2 INVITE"), false);
    let ladder = |entries: &[(Dir, String, bool)]| {
        pending_invites(&record(entries).legs().remove("A").expect("the leg was recorded"))
    };
    let awaited = |proceeding| PendingInvite { cseq: 2, sent_at_us: 3, proceeding };

    assert_eq!(
        ladder(std::slice::from_ref(&invite)),
        [PendingInvite { cseq: 1, sent_at_us: 0, proceeding: false }]
    );
    assert_eq!(ladder(&[invite.clone(), ok.clone(), ack.clone()]), [], "the 2xx answered it");
    let calling = [invite.clone(), ok.clone(), ack.clone(), reinvite.clone()];
    assert_eq!(ladder(&calling), [awaited(false)]);
    let mut proceeding = calling.to_vec();
    proceeding.push(trying);
    assert_eq!(ladder(&proceeding), [awaited(true)]);
    proceeding.push(rejected);
    assert_eq!(ladder(&proceeding), [], "a non-2xx final ends the wait too");
}

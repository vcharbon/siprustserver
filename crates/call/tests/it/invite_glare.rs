//! RFC 3261 §14.2 — the two INVITEs of the sender's or this side's own that
//! make a newcomer INVITE glare beside the relayed ones: an INVITE this side
//! sent on the dialog with no 2xx taken (491), and the sender's
//! dialog-creating INVITE still without a final when the newcomer names one
//! of its early dialogs (500).

use crate::common::representative_call;
use call::helpers::*;
use call::model::*;

const A_TAG: &str = "b2bua-to-tag-aleg-9876";
const ALICE_TAG: &str = "alice-from-tag-001";

fn a_id() -> DialogId {
    DialogId { our_tag: A_TAG.into(), peer_tag: ALICE_TAG.into() }
}

/// The fixture call with no INVITE relay or un-ACKed 2xx left on any dialog.
fn quiet_call() -> Call {
    let mut call = representative_call();
    let legs = std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut());
    for leg in legs {
        for d in leg.dialogs.iter_mut() {
            d.requests.clear();
            d.sent_invite = None;
            d.answered_invite = None;
        }
    }
    call
}

#[test]
fn an_invite_this_side_sent_with_no_2xx_taken_keeps_the_dialog_open() {
    let mut d = quiet_call().a_leg.dialogs.by_tags(A_TAG, ALICE_TAG).expect("a dialog").clone();
    assert!(!invite_transaction_open(&d), "a quiet dialog is free");
    d.sent_invite = Some(RequestRecord {
        origin: RequestOrigin::Own,
        sent: Some(SentRequest {
            branch: "z9hG4bK-own-reinvite".into(),
            image: Vec::new(),
            destination: HostPort { host: "caller.invalid".into(), port: 5060 },
        }),
        ..RequestRecord::relayed("INVITE", 7, 0)
    });
    assert!(invite_transaction_open(&d), "this side's INVITE awaits its final");
    sent_exchange_mut(&mut d).expect("an INVITE's record").ack_branch =
        Some("z9hG4bK-own-ack".into());
    assert!(!invite_transaction_open(&d), "its 2xx armed the ACK: the transaction is over");
}

#[test]
fn an_invite_on_the_early_dialog_of_an_unanswered_invite_meets_glare_as_unanswered() {
    let mut call = quiet_call();
    assert!(call.a_leg.outbound_invite().is_none(), "the caller's INVITE was received here");
    *call.a_leg.invite_final_sent_mut() = Some(200);
    assert!(!invite_glare(&call, "a", Some(&a_id())), "an answered caller is free");
    *call.a_leg.invite_final_sent_mut() = None;
    assert!(invite_glare(&call, "a", Some(&a_id())));
    assert!(sender_invite_unanswered(&call, "a", Some(&a_id())), "500, §14.2 first clause");
    assert!(!invite_glare(&call, "a", None), "a request naming no dialog is no newcomer");
}

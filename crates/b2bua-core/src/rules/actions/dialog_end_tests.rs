//! A dialog's end is written once, and a peer's BYE on a dialog no 2xx
//! established ends it as early.

use call::{Dialog, DialogEnd, DialogState, EndingBy, Leg};

use super::end_one;

fn leg_with(state: DialogState) -> Leg {
    let mut leg = call::testkit::dialled_leg("b-1", "own");
    leg.dialogs.insert(Dialog::new("own", "fork", state, 1));
    leg
}

fn end_of(leg: &Leg) -> &DialogState {
    &leg.dialogs.by_tags("own", "fork").expect("the dialog").state
}

fn id() -> Option<call::DialogId> {
    Some(call::DialogId { our_tag: "own".into(), peer_tag: "fork".into() })
}

#[test]
fn a_peer_bye_on_a_dialog_no_2xx_established_ends_it_early() {
    for early in [DialogState::Early, DialogState::Ending(EndingBy::Cancel)] {
        let mut leg = leg_with(early);
        end_one(&mut leg, id(), DialogEnd::PeerBye);
        assert_eq!(end_of(&leg), &DialogState::Ended(DialogEnd::EarlyPeerBye));
    }
    let mut leg = leg_with(DialogState::Confirmed);
    end_one(&mut leg, id(), DialogEnd::PeerBye);
    assert_eq!(end_of(&leg), &DialogState::Ended(DialogEnd::PeerBye));
}

#[test]
fn an_ended_dialog_keeps_its_first_end() {
    let rejected = DialogState::Ended(DialogEnd::Rejected(487));
    let mut leg = leg_with(rejected.clone());
    end_one(&mut leg, id(), DialogEnd::PeerBye);
    assert_eq!(end_of(&leg), &rejected);
}

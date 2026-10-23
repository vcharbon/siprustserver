//! Any dialog of a leg, for the round-trip property: its identity, state and
//! sequences, and what the B2BUA keeps on it.

use call::model::*;
use proptest::prelude::*;

use super::provisional::arb_owed_answer;
use super::request_record::{arb_invite_record, arb_pending_request};
use super::sdp_session::arb_sdp_session;
use super::subscription::{arb_queued_request, arb_subscription};
use super::{arb_bytes, arb_tag, arb_uri};

/// Boxed, as `arb_leg` is: a debug build otherwise holds the whole value tree
/// of a call on the test thread's stack.
pub(super) fn arb_dialog() -> BoxedStrategy<Dialog> {
    let sip = (
        arb_tag(),
        arb_tag(),
        arb_dialog_state(),
        arb_uri(),
        any::<i64>(),
        proptest::collection::vec("[a-z0-9 /.:;=<>-]{1,40}", 0..3),
        proptest::collection::vec(
            ("(a|b-[0-9]{1,2})", arb_tag())
                .prop_map(|(leg_id, peer_tag)| PeerDialog { leg_id, peer_tag }),
            0..2,
        ),
    );
    let ext = (
        proptest::option::of(any::<i64>()),
        proptest::collection::vec(arb_pending_request(), 0..3),
        proptest::option::of(arb_invite_record()),
        proptest::option::of(arb_invite_record()),
        proptest::option::of(arb_bytes(80)),
        (
            proptest::option::of(arb_sdp_session()),
            proptest::option::of(arb_owed_answer()),
            proptest::collection::vec(arb_subscription(), 0..2),
            proptest::collection::vec(arb_queued_request(), 0..3),
        ),
        (any::<bool>(), proptest::option::of(arb_tag())),
    );
    (sip, ext)
        .prop_map(
            |(
                (our_tag, peer_tag, state, remote_target, our_cseq, route_set, fronts),
                (
                    peer_cseq,
                    requests,
                    sent_invite,
                    answered_invite,
                    cached_sdp,
                    (sdp_session, owed_answer, subscriptions, queued),
                    (shown, shows_media),
                ),
            )| Dialog {
                our_tag,
                peer_tag,
                state,
                our_cseq,
                peer_cseq,
                remote_target,
                route_set,
                fronts,
                shown,
                shows_media,
                requests,
                sent_invite,
                answered_invite,
                cached_sdp,
                sdp_session,
                owed_answer,
                subscriptions,
                queued,
            },
        )
        .boxed()
}

/// Any dialog state, a retired one with open requests.
fn arb_dialog_state() -> impl Strategy<Value = DialogState> {
    let end = prop_oneof![
        any::<u16>().prop_map(DialogEnd::Rejected),
        Just(DialogEnd::PeerBye),
        Just(DialogEnd::EarlyPeerBye),
        proptest::option::of(any::<u16>())
            .prop_map(|final_status| DialogEnd::OurBye { final_status }),
    ];
    let retired = proptest::collection::vec(arb_pending_request(), 0..2)
        .prop_map(|open_requests| RetiredDialog { open_requests });
    prop_oneof![
        Just(DialogState::Early),
        Just(DialogState::Confirmed),
        Just(DialogState::Ending(EndingBy::Bye)),
        Just(DialogState::Ending(EndingBy::Cancel)),
        end.prop_map(DialogState::Ended),
        retired.prop_map(DialogState::Retired),
    ]
}

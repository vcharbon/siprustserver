//! Any leg of a call, for the round-trip property: its identity, its
//! dialogs and what the B2BUA keeps on it.

use call::model::*;
use proptest::prelude::*;

use super::dialog::arb_dialog;
use super::records::{arb_message_ring, arb_ring};
use super::request_record::arb_invite_record;
use super::sdp_session::arb_sdp_session;
use super::{arb_ext, arb_tag, arb_uri};

fn arb_remote_info() -> impl Strategy<Value = RemoteInfo> {
    ("[a-z0-9.]{1,15}", any::<u16>()).prop_map(|(address, port)| RemoteInfo { address, port })
}

fn arb_leg_kind() -> impl Strategy<Value = LegKind> {
    prop_oneof![
        Just(LegKind::A),
        Just(LegKind::Destination),
        Just(LegKind::Media),
        Just(LegKind::TransferTarget),
    ]
}

/// Boxed: a value tree this size, held by value in every enclosing tuple, is
/// past the 2 MiB test-thread stack of a debug build.
pub(super) fn arb_leg() -> BoxedStrategy<Leg> {
    (
        "(a|b-[0-9]{1,2})",
        arb_tag(),
        (proptest::option::of(arb_invite_record()), proptest::option::of(arb_tag())),
        arb_remote_info(),
        proptest::collection::vec(arb_dialog(), 0..3),
        (
            proptest::option::of(arb_uri()),
            proptest::option::of(arb_uri()),
            proptest::option::of(arb_uri()),
            arb_uri(),
        ),
        (
            arb_ext(),
            proptest::option::of(arb_leg_kind()),
            proptest::option::of(any::<bool>()),
            proptest::option::of(any::<bool>()),
        ),
        arb_message_ring(),
        arb_sdp_session(),
        (proptest::option::of((0usize..4).prop_map(AttemptId)), arb_ring()),
    )
        .prop_map(
            |(
                leg_id,
                call_id,
                (invite, own_tag),
                source,
                dialogs,
                (local_uri, remote_uri, invite_request_uri, contact),
                (ext, kind, adopted, in_session_timer),
                messages,
                sdp_session,
                (attempt, ring),
            )| Leg {
                leg_id,
                call_id,
                source,
                dialogs: dialogs.into(),
                local_uri,
                remote_uri,
                contact,
                invite_request_uri,
                invite,
                own_tag,
                ext,
                kind,
                adopted,
                in_session_timer,
                attempt,
                ring,
                messages,
                sdp_session,
            },
        )
        .boxed()
}

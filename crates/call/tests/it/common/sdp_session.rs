//! The proptest strategy of a leg's session state ([`LegSdpSession`]).

use call::model::LegSdpSession;
use proptest::prelude::*;

pub(super) fn arb_sdp_session() -> impl Strategy<Value = LegSdpSession> {
    (
        proptest::option::of("[a-z0-9 .]{1,40}"),
        proptest::collection::vec("[a-z]{1,8} [0-9]{1,5} [A-Z/]{1,8} [0-9 ]{1,12}", 0..3),
        proptest::collection::vec(proptest::option::of(0u32..4), 0..3),
        (proptest::option::of("(a|b-[0-9]{1,2})"), proptest::option::of("[a-z0-9 .]{1,40}")),
        proptest::option::of("(a|b-[0-9]{1,2})"),
        proptest::option::of("[0-9]{1,20}"),
        (any::<bool>(), any::<u32>(), any::<bool>()),
        proptest::option::of("[a-z0-9 =\n]{1,40}"),
    )
        .prop_map(
            |(
                sent_origin,
                sent_media,
                sent_slots,
                (sent_slots_author, restated_from),
                session_author,
                session_id,
                (restated, exchanges_opened, has_restated),
                sent_description,
            )| {
                LegSdpSession {
                    sent_origin,
                    sent_description,
                    sent_media,
                    sent_slots,
                    sent_slots_author,
                    restated_from,
                    session_author,
                    session_id,
                    restated,
                    exchanges_opened,
                    has_restated,
                }
            },
        )
}

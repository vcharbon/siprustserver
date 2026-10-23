//! The request-record strategies of the round-trip property (ADR-0055 §2):
//! every origin, usage and sent transaction a dialog's record may hold.

use call::model::*;
use proptest::prelude::*;

use super::{arb_tag, arb_uri};

fn arb_origin() -> impl Strategy<Value = RequestOrigin> {
    prop_oneof![Just(RequestOrigin::Relayed), Just(RequestOrigin::Own), Just(RequestOrigin::Held)]
}

fn arb_usage() -> impl Strategy<Value = DialogUsage> {
    prop_oneof![
        Just(DialogUsage::Session),
        ("[a-z]{3,8}", proptest::option::of("[0-9]{1,4}"))
            .prop_map(|(event, id)| DialogUsage::Subscription { event, id }),
    ]
}

fn arb_sent() -> impl Strategy<Value = SentRequest> {
    (arb_tag(), proptest::collection::vec(any::<u8>(), 0..64), any::<u16>()).prop_map(
        |(branch, image, port)| SentRequest {
            branch,
            image,
            destination: HostPort { host: "127.0.0.1".into(), port },
        },
    )
}

pub(super) fn arb_pending_request() -> impl Strategy<Value = RequestRecord> {
    (
        (
            "[A-Z]{3,8}",
            arb_origin(),
            arb_usage(),
            any::<i64>(),
            proptest::option::of(arb_sent()),
            proptest::option::of(any::<i64>()),
            proptest::option::of(arb_tag()),
        ),
        (
            any::<i64>(),
            proptest::collection::vec("[a-z0-9 /.:;=-]{1,40}", 0..3),
            arb_tag(),
            arb_uri(),
            arb_uri(),
            proptest::option::of("[0-9]{1,10}(\\.[0-9]{1,3})?"),
        ),
    )
        .prop_map(
            |(
                (method, origin, usage, outbound_cseq, sent, deadline_ms, author),
                (
                    inbound_cseq,
                    source_vias,
                    source_call_id,
                    source_from,
                    source_to,
                    source_timestamp,
                ),
            )| {
                RequestRecord {
                    method,
                    origin,
                    usage,
                    outbound_cseq,
                    sent,
                    deadline_ms,
                    inbound_cseq,
                    source_vias,
                    source_call_id,
                    source_from,
                    source_to,
                    cancelled: false,
                    offered_100rel: false,
                    offered: false,
                    source_timestamp,
                    relayed_by: Default::default(),
                    author,
                }
            },
        )
}

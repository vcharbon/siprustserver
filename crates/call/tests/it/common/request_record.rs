//! The request-record strategies of the round-trip property (ADR-0055 §2):
//! every origin, usage and sent transaction a dialog's record may hold.

use call::model::*;
use proptest::prelude::*;

use super::emission::{arb_retained_emission, arb_unacked_2xx};
use super::{arb_tag, arb_uri};

fn arb_origin() -> impl Strategy<Value = RequestOrigin> {
    prop_oneof![
        Just(RequestOrigin::Relayed),
        Just(RequestOrigin::Own),
        Just(RequestOrigin::Held),
        Just(RequestOrigin::Received),
    ]
}

fn arb_usage() -> impl Strategy<Value = DialogUsage> {
    prop_oneof![
        Just(DialogUsage::Session),
        ("[a-z]{3,8}", proptest::option::of("[0-9]{1,4}"))
            .prop_map(|(event, id)| DialogUsage::Subscription { event, id }),
    ]
}

pub(super) fn arb_sent() -> impl Strategy<Value = SentRequest> {
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
                    invite: None,
                }
            },
        )
}

/// Any INVITE exchange: the ACK its 2xx drew or awaits, the 2xx it answered,
/// a BYE held behind that 2xx's ACK, the dialling facts or the final sent.
fn arb_exchange() -> impl Strategy<Value = InviteExchange> {
    (
        (
            proptest::option::of(arb_tag()),
            proptest::option::of(arb_retained_emission()),
            proptest::option::of(any::<i64>()),
            proptest::option::of(arb_unacked_2xx()),
        ),
        (
            proptest::option::of((arb_tag(), any::<i64>(), proptest::option::of(100u16..700))),
            proptest::option::of(100u16..700),
            proptest::option::of(200u16..300),
            proptest::option::of(proptest::option::of(arb_tag())),
        ),
    )
        .prop_map(
            |(
                (ack_branch, emitted_ack, awaited_ack_cseq, answered_2xx),
                (dialled, final_sent, answer_preempted, bye_reason),
            )| InviteExchange {
                ack_branch,
                emitted_ack,
                awaited_ack_cseq,
                answered_2xx,
                bye_behind_ack: bye_reason.map(|reason| QueuedRequest::Bye {
                    reason,
                    headers: Vec::new(),
                    body: Vec::new(),
                    content_type: None,
                    max_forwards: None,
                }),
                dialled: dialled.map(|(tag, sent_at, final_received)| OutboundInvite {
                    final_received,
                    ..OutboundInvite::new(tag, sent_at)
                }),
                final_sent,
                answer_preempted,
            },
        )
}

/// Any INVITE's record: a request record carrying its exchange.
pub(super) fn arb_invite_record() -> impl Strategy<Value = RequestRecord> {
    (arb_pending_request(), arb_exchange(), any::<bool>()).prop_map(
        |(record, exchange, cancelled)| RequestRecord {
            method: "INVITE".into(),
            cancelled,
            invite: Some(exchange),
            ..record
        },
    )
}

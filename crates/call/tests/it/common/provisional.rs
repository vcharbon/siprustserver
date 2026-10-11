//! The reliable-provisional strategies of the round-trip property: a
//! provisional record (RFC 3262) and the answer a dialog owes (RFC 3264).

use call::model::*;
use proptest::prelude::*;

use super::{arb_bytes, arb_retained_emission, arb_tag};

/// The answer a dialog owes its responder (RFC 3264 §4), in each place it
/// rides.
pub(super) fn arb_owed_answer() -> impl Strategy<Value = OwedAnswer> {
    (0u8..3, any::<i64>(), any::<u32>(), arb_bytes(80)).prop_map(|(kind, ack_cseq, rseq, offer)| {
        match kind {
            0 => OwedAnswer::InAck { ack_cseq, offer },
            1 => OwedAnswer::InPrack { rseq: RSeq(rseq), offer },
            _ => OwedAnswer::DeferredToAck { rseq: RSeq(rseq), offer },
        }
    })
}

/// A reliable provisional the call received (RFC 3262): owed its PRACK or
/// settled either way, shown or not, its §3 ladder at any step.
pub(super) fn arb_provisional() -> impl Strategy<Value = Provisional> {
    let id = ("(a|b-[0-9]{1,2})", arb_tag(), any::<u32>(), any::<u32>()).prop_map(
        |(leg, responder_tag, cseq, rseq)| ProvisionalId {
            leg,
            responder_tag,
            invite_cseq: CSeq(cseq),
            rseq: RSeq(rseq),
        },
    );
    let responder = prop_oneof![
        Just(ResponderAck::Pending),
        Just(ResponderAck::RelayedPrack),
        arb_tag().prop_map(|branch| ResponderAck::OwnPrack { branch }),
        Just(ResponderAck::DeferredToAck),
        Just(ResponderAck::Waived),
    ];
    let ladder = prop_oneof![
        Just(Ladder::Unsent),
        arb_retained_emission().prop_map(Ladder::Repeating),
        Just(Ladder::Acknowledged),
        Just(Ladder::Ceased(Ceased::Final)),
        Just(Ladder::Ceased(Ceased::GiveUp)),
    ];
    let shown = proptest::option::of(
        (arb_tag(), any::<u32>(), any::<u32>(), any::<bool>(), ladder).prop_map(
            |(tag, rseq, cseq, carried_sdp, ladder)| Shown {
                tag,
                rseq: RSeq(rseq),
                invite_cseq: CSeq(cseq),
                carried_sdp,
                ladder,
            },
        ),
    );
    (id, any::<bool>(), responder, shown).prop_map(|(id, responder_sdp, responder, shown)| {
        Provisional { id, responder_sdp, responder, shown }
    })
}

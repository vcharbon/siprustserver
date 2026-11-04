//! What a dialog keeps of the messages this stack sent, for the round-trip
//! property: the retained emissions its ladders repeat and the 2xx awaiting
//! its ACK.

use call::model::*;
use proptest::prelude::*;
use sip_retransmit::Class;

use super::{arb_bytes, arb_tag, paced_emission};

/// Any retained emission: bytes and destination under either repeat shape,
/// a paced one standing anywhere on the first rungs of any dialog-level class.
pub(super) fn arb_retained_emission() -> impl Strategy<Value = RetainedEmission> {
    let paced = (
        arb_bytes(120),
        "[a-z0-9.]{3,20}",
        any::<u16>(),
        prop_oneof![Just(Class::Final2xx), Just(Class::ReliableProvisional)],
        1u32..6,
    )
        .prop_map(|(bytes, host, port, class, rung)| {
            paced_emission(&bytes, (&host, port), class, rung)
        });
    let on_trigger =
        (arb_bytes(120), "[a-z0-9.]{3,20}", any::<u16>()).prop_map(|(bytes, host, port)| {
            RetainedEmission::on_trigger(bytes, (host, port), Repeated::request("ACK"))
        });
    prop_oneof![paced, on_trigger]
}

/// A 2xx still awaiting its ACK: the key the ACK carries plus the retained
/// emission on the §13.3.1.4 ladder.
pub(super) fn arb_unacked_2xx() -> impl Strategy<Value = Unacked2xx> {
    (arb_tag(), any::<i64>(), arb_retained_emission())
        .prop_map(|(dialog_tag, cseq, emission)| Unacked2xx { dialog_tag, cseq, emission })
}

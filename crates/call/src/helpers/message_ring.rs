//! The message-ring append: the one writer of a leg's
//! [`MessageRing`](crate::model::MessageRing) and of the call-wide sequence
//! every entry draws its `seq` from.

use crate::model::{Call, MessageEntry};

use super::lens::update_leg;

/// Append `entry` to `leg_id`'s ring under `cap`, stamping it with the next
/// call-wide `seq` and the count of decisions applied so far
/// (`Call::decision_ordinal`). `Call::message_seq` is the `seq` of the last
/// message recorded on any leg, so the two rings of a call interleave by it.
/// A cap of `0` records nothing and moves nothing; an unknown leg is left
/// alone.
pub fn record_message(mut call: Call, leg_id: &str, cap: usize, mut entry: MessageEntry) -> Call {
    if cap == 0 || super::find_leg(&call, leg_id).is_none() {
        return call;
    }
    call.message_seq += 1;
    entry.seq = call.message_seq;
    entry.decision_ordinal = call.decision_ordinal;
    update_leg(call, leg_id, |leg| leg.messages.push(entry, cap))
}

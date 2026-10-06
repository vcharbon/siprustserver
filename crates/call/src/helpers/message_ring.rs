//! The message-ring append and the turn seal: the one writer of a leg's
//! [`MessageRing`](crate::model::MessageRing), of the call-wide sequence
//! every entry draws its `seq` from and of the turn counter every entry is
//! numbered by.

use crate::model::{Call, MessageEntry};

use super::lens::update_leg;

/// Append `entry` to `leg_id`'s ring under `cap`, stamping it with the next
/// call-wide `seq`, the turn being handled (`Call::message_turn`) and the
/// count of decisions applied so far (`Call::decision_ordinal`).
/// `Call::message_seq` is the `seq` of the last message recorded on any leg,
/// so the two rings of a call interleave by it.
/// A cap of `0` records nothing and moves nothing; an unknown leg is left
/// alone.
pub fn record_message(mut call: Call, leg_id: &str, cap: usize, mut entry: MessageEntry) -> Call {
    if cap == 0 || super::find_leg(&call, leg_id).is_none() {
        return call;
    }
    call.message_seq += 1;
    entry.seq = call.message_seq;
    entry.turn = call.message_turn;
    entry.decision_ordinal = call.decision_ordinal;
    update_leg(call, leg_id, |leg| leg.messages.push(entry, cap))
}

/// Close the turn being handled once all its entries are on the ring: the
/// counter moves to the next turn's number, whether this turn appended an
/// entry or not — one turn, one number.
pub fn seal_turn(mut call: Call) -> Call {
    call.message_turn += 1;
    call
}

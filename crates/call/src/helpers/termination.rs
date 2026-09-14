//! The termination-record writes: the one writer of
//! [`Call::termination`](crate::model::Call) and of the message-ring cut it
//! carries.

use crate::model::{Call, Termination, TerminationCause};

/// Record that the call began terminating at `at_ms` under `cause`, caused
/// by `by_leg`'s message or timer. The first termination stands: a call
/// already carrying a record keeps it, whatever ends it again later (the
/// safety timer forcing a wedged teardown terminal, a reaper verdict on a
/// terminating call).
pub fn record_termination(
    mut call: Call,
    at_ms: i64,
    cause: TerminationCause,
    by_leg: Option<String>,
) -> Call {
    if call.termination.is_none() {
        call.termination = Some(Termination { at_ms, cause, by_leg, last_seq: 0 });
    }
    call
}

/// Close the terminating turn's message-ring cut: a record written this
/// turn (its `last_seq` still `0`) takes the `seq` of the last message
/// recorded on the call, once the turn's own emissions are on the ring. A
/// record already cut is left alone; a call without one is left alone.
pub fn seal_termination_seq(mut call: Call) -> Call {
    if let Some(t) = call.termination.as_mut() {
        if t.last_seq == 0 {
            t.last_seq = call.message_seq;
        }
    }
    call
}

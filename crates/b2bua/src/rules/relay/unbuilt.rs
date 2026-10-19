//! The record of a message this stack composed and could not build: none is
//! sent, and each drop is logged where it happens (ADR-0051 §5).

use sip_message::generators::GenerateError;

/// Log that `what` toward `leg_id` on `call_ref` was not sent, because the
/// lines it states or echoes make no message.
pub(crate) fn unbuilt(call_ref: &str, leg_id: &str, what: &str, e: &GenerateError) {
    tracing::warn!(%call_ref, %leg_id, %what, error = %e, "message not sent: its lines make none");
}

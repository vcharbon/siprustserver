//! Dependency log targets that carry a received message's payload in their
//! non-error events. Stdout never renders those events: a payload is data
//! (possibly personal), not lifecycle. Their errors still reach stdout.

use tracing::Level;

/// Targets whose warn-and-below events embed a message payload.
const PAYLOAD_TARGETS: &[&str] = &[
    // The AMQP client logs every returned (unroutable) publish, body included.
    "lapin::returned_messages",
];

/// Whether stdout must not render this event.
pub fn carries_payload(meta: &tracing::Metadata<'_>) -> bool {
    *meta.level() > Level::ERROR && PAYLOAD_TARGETS.contains(&meta.target())
}

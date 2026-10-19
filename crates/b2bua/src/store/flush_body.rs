//! The body a replication flush writes: the call encoded with the store's
//! authoritative topology embedded, not only in the frame meta. The `Call` a
//! flush is handed may be a pre-`update` clone whose `(p,b)` counters are one
//! bump stale, and a node that hydrates from the body branches at the
//! topology the body holds (ADR-0014).

use call::{Call, CallBodyCodec, CallTopology, MsgpackCodec};

/// `call` encoded with `stored` as its topology when the two differ; `None`,
/// logged, when the body does not encode and the flush is skipped.
pub(super) fn encode(
    codec: &MsgpackCodec,
    call: &Call,
    stored: Option<&CallTopology>,
) -> Option<Vec<u8>> {
    let encoded = match (call.topology.as_ref(), stored) {
        (Some(t), Some(s)) if t != s => {
            let mut consistent = call.clone();
            consistent.topology = Some(s.clone());
            codec.encode(&consistent)
        }
        _ => codec.encode(call),
    };
    match encoded {
        Ok(body) => Some(body),
        Err(e) => {
            tracing::error!(call_ref = %call.call_ref, error = %e, "flush skipped: the call body did not encode");
            None
        }
    }
}
